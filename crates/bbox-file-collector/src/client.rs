//! The `/internal/file-source/v1/*` wire client.
//!
//! Thin by design: it owns the base URL, the bearer, and the error taxonomy,
//! and nothing else. Every decision about WHAT to publish lives in
//! [`crate::cycle`], which is written against the [`PublicationSink`] trait so
//! those decisions are testable without a socket.
//!
//! Two details are load-bearing and easy to get wrong:
//!
//! 1. **The blob PUT sends an exact `Content-Length`.** The route refuses a
//!    blob whose declared length disagrees with the size the manifest
//!    declared for that hash, before it reads a byte. Sending a chunked body
//!    would be refused as `content_length_required`, so the bytes go as a
//!    sized body.
//! 2. **`complete` and `finalize` send NO body.** Both routes cap the body at
//!    one byte, so a helpfully-serialized `{}` is a 413 rather than a
//!    protocol error a reader would recognize.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bbox_file_source::{
    BeginFileUploadRequestV1, BeginFileUploadResponseV1, ErrorResponse,
    FileCatalogOnboardRequestV1, FileCatalogOnboardResponseV1, FileGenerationStatusV1,
    FileManifestEntryV1, FileManifestPageV1, FinalizeFileUploadResponseV1, MissingFileBlobsPageV1,
};
use reqwest::{Client, StatusCode};

use crate::cycle::PublicationSink;

/// An authenticated client for one daemon.
pub struct FileSourceClient {
    base_url: String,
    bearer: String,
    http: Client,
}

impl std::fmt::Debug for FileSourceClient {
    /// Written by hand because this type RETAINS a bearer. The derive would
    /// put a live producer credential into every panic message and every
    /// tracing field that rendered it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileSourceClient")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .finish()
    }
}

impl FileSourceClient {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into();
        let base_url = base_url.trim_end_matches('/').to_string();
        if base_url.is_empty() {
            bail!("a file-source client needs a base URL");
        }
        Ok(Self {
            base_url,
            bearer: bearer.into(),
            http: Client::builder()
                .build()
                .map_err(|error| anyhow!("building the file-source HTTP client: {error}"))?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    /// Present probed remote facts and find-or-create the catalog project.
    ///
    /// Safe to drive before every cycle: the daemon side is find-or-create, so
    /// a producer that always onboards is idempotent rather than duplicating.
    pub async fn onboard(
        &self,
        request: &FileCatalogOnboardRequestV1,
    ) -> Result<FileCatalogOnboardResponseV1> {
        let response = self
            .http
            .post(self.url("/internal/file-source/v1/catalog/onboard"))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await?;
        decode(response).await
    }
}

/// Decode a response, mapping the wire error taxonomy onto an anyhow error
/// that names the daemon's own code and message.
///
/// The code is preserved verbatim because it is the actionable half: an
/// operator reading `scope_pending_onboarding` knows to onboard, and one
/// reading `unsupported_contract` knows to deploy the satellite.
async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("decoding a {status} file-source response: {error}"));
    }
    match serde_json::from_slice::<ErrorResponse>(&bytes) {
        Ok(error) => bail!(
            "file-source request failed with {status} {}: {}",
            error.code,
            error.message
        ),
        Err(_) => bail!("file-source request failed with {status}"),
    }
}

/// Expect a no-content success, with the same error decoding.
async fn expect_no_content(response: reqwest::Response) -> Result<()> {
    let status = response.status();
    if status == StatusCode::NO_CONTENT || status.is_success() {
        return Ok(());
    }
    let bytes = response.bytes().await?;
    match serde_json::from_slice::<ErrorResponse>(&bytes) {
        Ok(error) => bail!(
            "file-source request failed with {status} {}: {}",
            error.code,
            error.message
        ),
        Err(_) => bail!("file-source request failed with {status}"),
    }
}

#[async_trait]
impl PublicationSink for FileSourceClient {
    async fn begin(&self, request: &BeginFileUploadRequestV1) -> Result<BeginFileUploadResponseV1> {
        let response = self
            .http
            .post(self.url("/internal/file-source/v1/uploads"))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await?;
        decode(response).await
    }

    async fn put_manifest_page(
        &self,
        upload_id: &str,
        page: u64,
        entries: &[FileManifestEntryV1],
    ) -> Result<()> {
        let response = self
            .http
            .put(self.url(&format!(
                "/internal/file-source/v1/uploads/{upload_id}/manifest/{page}"
            )))
            .bearer_auth(&self.bearer)
            .json(&FileManifestPageV1 {
                entries: entries.to_vec(),
            })
            .send()
            .await?;
        expect_no_content(response).await
    }

    async fn complete_manifest(&self, upload_id: &str) -> Result<MissingFileBlobsPageV1> {
        let response = self
            .http
            .post(self.url(&format!(
                "/internal/file-source/v1/uploads/{upload_id}/manifest/complete"
            )))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        decode(response).await
    }

    async fn missing_blobs(
        &self,
        upload_id: &str,
        cursor: Option<&str>,
    ) -> Result<MissingFileBlobsPageV1> {
        let mut request = self
            .http
            .get(self.url(&format!(
                "/internal/file-source/v1/uploads/{upload_id}/missing"
            )))
            .bearer_auth(&self.bearer);
        if let Some(cursor) = cursor {
            request = request.query(&[("cursor", cursor)]);
        }
        decode(request.send().await?).await
    }

    async fn put_blob(&self, upload_id: &str, hash: &str, bytes: Vec<u8>) -> Result<()> {
        let length = bytes.len() as u64;
        let response = self
            .http
            .put(self.url(&format!(
                "/internal/file-source/v1/uploads/{upload_id}/blobs/{hash}"
            )))
            .bearer_auth(&self.bearer)
            .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
            .header(reqwest::header::CONTENT_LENGTH, length)
            .body(bytes)
            .send()
            .await?;
        expect_no_content(response).await
    }

    async fn finalize(&self, upload_id: &str) -> Result<FinalizeFileUploadResponseV1> {
        let response = self
            .http
            .post(self.url(&format!(
                "/internal/file-source/v1/uploads/{upload_id}/finalize"
            )))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        decode(response).await
    }

    async fn status(&self, generation_id: &str) -> Result<FileGenerationStatusV1> {
        let response = self
            .http
            .get(self.url(&format!(
                "/internal/file-source/v1/generations/{generation_id}/status"
            )))
            .bearer_auth(&self.bearer)
            .send()
            .await?;
        decode(response).await
    }
}

//! HTTP client for the corpus host's `POST /internal/records`.
//!
//! Mirrors the shipping model of `fleetd`'s `BlackboxRecordHttpClient`: a
//! reqwest client with connect/request timeouts, a bearer `Authorization`
//! header from the shared service token, and a typed `RecordIngestRequest`
//! body. The one addition is that gap/overlap rejections are surfaced as a
//! distinct [`IngestError::StreamCursorRejected`] variant so the shipper can
//! resync-and-retry.

use std::time::Duration;

use bro_capabilities::{RecordEnvelope, RecordIngestReceipt, RecordIngestRequest};
use bro_rpc::ServiceToken;
use reqwest::header::AUTHORIZATION;

/// Server error codes that mean "your local cursor disagrees with the server's
/// acknowledged tail": recover by resyncing and retrying.
const GAP_CODE: &str = "record_ingest.transcript_increment_gap";
const OVERLAP_CODE: &str = "record_ingest.transcript_increment_overlap";

#[derive(Debug)]
pub enum IngestError {
    /// Transport failure (connection refused, timeout, DNS). Retry next tick
    /// with backoff; the source file is the durable backlog.
    Unavailable(String),
    /// The server rejected an increment because the submitted byte range does
    /// not abut its acknowledged tail. Resync and retry once.
    StreamCursorRejected { code: String, message: String },
    /// Any other non-success response (bad request, conflict, 5xx).
    Http { status: u16, body: String },
    /// The success body could not be decoded.
    Decode(String),
}

impl IngestError {
    pub fn is_stream_cursor_rejection(&self) -> bool {
        matches!(self, IngestError::StreamCursorRejected { .. })
    }
}

impl std::fmt::Display for IngestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IngestError::Unavailable(msg) => write!(f, "corpus unavailable: {msg}"),
            IngestError::StreamCursorRejected { code, message } => {
                write!(f, "stream cursor rejected ({code}): {message}")
            }
            IngestError::Http { status, body } => write!(f, "corpus returned {status}: {body}"),
            IngestError::Decode(msg) => write!(f, "receipt decode failed: {msg}"),
        }
    }
}

impl std::error::Error for IngestError {}

#[derive(Clone)]
pub struct IngestClient {
    client: reqwest::Client,
    endpoint: String,
    authorization: reqwest::header::HeaderValue,
}

impl IngestClient {
    pub fn new(corpus_url: &str, token: &ServiceToken, timeout: Duration) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(timeout)
            .build()?;
        Ok(Self {
            client,
            endpoint: format!("{}/internal/records", corpus_url.trim_end_matches('/')),
            authorization: token.authorization_header(),
        })
    }

    /// POST a batch (possibly empty, for a resync probe) and return the receipt.
    pub async fn ingest(
        &self,
        records: Vec<RecordEnvelope>,
    ) -> Result<RecordIngestReceipt, IngestError> {
        let response = self
            .client
            .post(&self.endpoint)
            .header(AUTHORIZATION, self.authorization.clone())
            .json(&RecordIngestRequest { records })
            .send()
            .await
            .map_err(|e| IngestError::Unavailable(e.to_string()))?;

        let status = response.status();
        if status.is_success() {
            return response
                .json::<RecordIngestReceipt>()
                .await
                .map_err(|e| IngestError::Decode(e.to_string()));
        }

        let body = response.text().await.unwrap_or_default();
        if let Some((code, message)) = parse_error_body(&body)
            && (code == GAP_CODE || code == OVERLAP_CODE)
        {
            return Err(IngestError::StreamCursorRejected { code, message });
        }
        Err(IngestError::Http {
            status: status.as_u16(),
            body,
        })
    }
}

/// The corpus service renders errors as `{"error":{"code","message",...}}`.
fn parse_error_body(body: &str) -> Option<(String, String)> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    let code = error.get("code")?.as_str()?.to_string();
    let message = error
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string();
    Some((code, message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gap_error_body() {
        let body = r#"{"error":{"code":"record_ingest.transcript_increment_gap","message":"stream x expected 10","retryable":false}}"#;
        let (code, message) = parse_error_body(body).unwrap();
        assert_eq!(code, GAP_CODE);
        assert!(message.contains("expected 10"));
    }

    #[test]
    fn non_error_body_is_none() {
        assert!(parse_error_body("plain text").is_none());
        assert!(parse_error_body("{}").is_none());
    }
}

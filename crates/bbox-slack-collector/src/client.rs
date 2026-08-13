//! The `/internal/conversation-source/v1/*` wire client.
//!
//! Thin by design, exactly like the file lane's: it owns the base URL, the
//! producer bearer, and the error taxonomy, and nothing else. Every decision
//! about WHAT to publish lives in [`crate::cycle`], which is written against
//! [`ConversationSink`] so those decisions are testable without a socket.
//!
//! Two details are load-bearing:
//!
//! 1. **The two GET verbs carry the scope in the QUERY STRING**, both halves,
//!    because the corpus host compares the whole [`ConnectorScope`] against the
//!    grant and will not reconstruct a missing half.
//! 2. **The daemon's error CODE is preserved verbatim** in the surfaced error.
//!    It is the actionable part: `scope_pending_onboarding` means onboard,
//!    `scope_wrong_lane` means the grant names the file lane, and
//!    `scope_forbidden` means the operator never granted it. Collapsing them
//!    into "publish failed" sends an operator to edit the wrong file.

use anyhow::{Result, anyhow, bail};
use async_trait::async_trait;
use bbox_conversation_source::{
    ChannelRosterReceiptV1, ChannelRosterRequestV1, ConversationBatchReceiptV1,
    ConversationBatchV1, ConversationCatalogOnboardRequestV1, ConversationCatalogOnboardResponseV1,
    ConversationCursorsResponseV1, ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1,
    ConversationStatusResponseV1, ErrorResponse,
};
use bbox_corpus_core::project_catalog::ConnectorScope;
use reqwest::Client;

use crate::cycle::ConversationSink;

pub struct ConversationSourceClient {
    base_url: String,
    bearer: String,
    http: Client,
}

impl std::fmt::Debug for ConversationSourceClient {
    /// Written by hand because this type RETAINS a bearer. The derive would put
    /// a live producer credential into every panic message and every tracing
    /// field that rendered it.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConversationSourceClient")
            .field("base_url", &self.base_url)
            .field("bearer", &"<redacted>")
            .finish()
    }
}

impl ConversationSourceClient {
    pub fn new(base_url: impl Into<String>, bearer: impl Into<String>) -> Result<Self> {
        let base_url = base_url.into().trim_end_matches('/').to_string();
        crate::config::require_safe_transport(&base_url, "corpus_url")?;
        let bearer = bearer.into();
        if bearer.trim().is_empty() {
            bail!("the conversation-source client needs a producer bearer");
        }
        Ok(Self {
            base_url,
            bearer,
            http: Client::builder()
                // Redirect following is off (design 4.1): a redirect is a
                // credential-forwarding primitive and this request carries a
                // bearer.
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(|error| anyhow!("building the conversation-source HTTP client: {error}"))?,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }

    fn scope_query(scope: &ConnectorScope) -> [(&'static str, String); 2] {
        [
            (
                "connector_source_id",
                scope.connector_source_id().as_str().to_string(),
            ),
            ("connector_kind", scope.connector_kind().as_str().to_string()),
        ]
    }
}

#[async_trait]
impl ConversationSink for ConversationSourceClient {
    async fn onboard(
        &self,
        request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1> {
        let response = self
            .http
            .post(self.url("/internal/conversation-source/v1/catalog/onboard"))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await?;
        decode(response).await
    }

    async fn post_channels(
        &self,
        request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1> {
        let response = self
            .http
            .post(self.url("/internal/conversation-source/v1/channels"))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await?;
        decode(response).await
    }

    async fn cursors(&self, scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1> {
        let response = self
            .http
            .get(self.url("/internal/conversation-source/v1/cursors"))
            .bearer_auth(&self.bearer)
            .query(&Self::scope_query(scope))
            .send()
            .await?;
        decode(response).await
    }

    async fn post_batch(&self, batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1> {
        let response = self
            .http
            .post(self.url("/internal/conversation-source/v1/batches"))
            .bearer_auth(&self.bearer)
            .json(batch)
            .send()
            .await?;
        decode(response).await
    }

    async fn post_revisions(
        &self,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        let response = self
            .http
            .post(self.url("/internal/conversation-source/v1/revisions"))
            .bearer_auth(&self.bearer)
            .json(request)
            .send()
            .await?;
        decode(response).await
    }

    async fn status(&self, scope: &ConnectorScope) -> Result<ConversationStatusResponseV1> {
        let response = self
            .http
            .get(self.url("/internal/conversation-source/v1/status"))
            .bearer_auth(&self.bearer)
            .query(&Self::scope_query(scope))
            .send()
            .await?;
        decode(response).await
    }
}

/// Decode a response, mapping the wire error taxonomy onto an error that names
/// the corpus host's own code and message.
async fn decode<T: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<T> {
    let status = response.status();
    let bytes = response.bytes().await?;
    if status.is_success() {
        return serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("decoding a {status} conversation-source response: {error}"));
    }
    match serde_json::from_slice::<ErrorResponse>(&bytes) {
        Ok(error) => bail!(
            "conversation-source request failed with {status} {}: {}",
            error.code,
            error.message
        ),
        Err(_) => bail!("conversation-source request failed with {status}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_loopback_plain_http_corpus_url_is_refused() {
        let error = ConversationSourceClient::new("http://corpus.example.com", "bearer")
            .unwrap_err()
            .to_string();
        assert!(error.contains("corpus_url"), "{error}");
    }

    #[test]
    fn the_debug_rendering_never_carries_the_bearer() {
        let client =
            ConversationSourceClient::new("http://127.0.0.1:7264", "producer-secret").unwrap();
        let rendered = format!("{client:?}");
        assert!(!rendered.contains("producer-secret"), "{rendered}");
        assert!(rendered.contains("<redacted>"), "{rendered}");
    }

    #[test]
    fn the_scope_query_carries_both_halves() {
        let scope = ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "slack").unwrap();
        let query = ConversationSourceClient::scope_query(&scope);
        assert_eq!(query[0].0, "connector_source_id");
        assert_eq!(query[0].1, "csrc_5f2c1d9a4b6e470e");
        assert_eq!(query[1].0, "connector_kind");
        assert_eq!(query[1].1, "slack");
    }
}

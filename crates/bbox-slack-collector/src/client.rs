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
//!
//! 3. **Transport failures are retried on a bounded ladder, refusals are not.**
//!    Connect errors, timeouts, and HTTP 5xx are blips -- a daemon `Recreate`
//!    or an ingress restart -- and one blip must not cost a whole cycle. A 4xx
//!    is the daemon's considered answer to THIS request, and retrying it would
//!    only delay the operator seeing it.

use std::time::Duration;

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

/// Retries after a transport failure or a 5xx, bounded so a hard-down corpus
/// still returns control to the watch loop within one cycle's lifetime.
const CORPUS_RETRIES: u32 = 3;
const CORPUS_RETRY_BACKOFF_SECS: [u64; 3] = [5, 15, 30];

pub struct ConversationSourceClient {
    base_url: String,
    bearer: String,
    http: Client,
    /// Injectable so the retry tests do not pay real seconds. Production gets
    /// [`CORPUS_RETRY_BACKOFF_SECS`].
    retry_backoff_secs: Vec<u64>,
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
                .map_err(|error| {
                    anyhow!("building the conversation-source HTTP client: {error}")
                })?,
            retry_backoff_secs: CORPUS_RETRY_BACKOFF_SECS.to_vec(),
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
            (
                "connector_kind",
                scope.connector_kind().as_str().to_string(),
            ),
        ]
    }

    /// Send one corpus call, retrying the failures that are blips.
    ///
    /// A failed `send` is transport by definition (connect, DNS, TLS, timeout)
    /// and a 5xx is the corpus side admitting it is unhealthy; both get the
    /// bounded ladder. Everything else, including every 4xx and every
    /// error-code body the daemon answered with, returns immediately: that is
    /// an answer, not a blip, and retrying it only delays the operator seeing
    /// it.
    async fn round_trip<T: serde::de::DeserializeOwned>(
        &self,
        describe: &str,
        request: impl Fn() -> reqwest::RequestBuilder,
    ) -> Result<T> {
        let mut attempt = 0_u32;
        loop {
            let failure = request().send().await;
            let retryable = match &failure {
                Err(_) => true,
                Ok(response) => response.status().is_server_error(),
            };
            if !retryable {
                return match failure {
                    Ok(response) => decode(response).await,
                    Err(error) => Err(anyhow!("{describe}: {error}")),
                };
            }
            if attempt >= CORPUS_RETRIES {
                // The last failure's BODY still matters: it is the only
                // diagnosis of which tier was answering, so it is flattened
                // into the surfaced message rather than left on a chain link
                // a single-line log would drop.
                return match failure {
                    Ok(response) => decode(response).await.map_err(|cause| {
                        anyhow!("{describe} kept failing after {attempt} retries: {cause}")
                    }),
                    Err(error) => Err(anyhow!(
                        "{describe} kept failing after {attempt} retries: {error}"
                    )),
                };
            }
            let rendered = match &failure {
                Ok(response) => response.status().to_string(),
                Err(error) => error.to_string(),
            };
            let delay_secs = self
                .retry_backoff_secs
                .get(attempt as usize)
                .copied()
                .unwrap_or(0);
            tracing::warn!(
                attempt = attempt + 1,
                status = %rendered,
                delay_secs,
                retry_budget = CORPUS_RETRIES,
                "corpus call failed with a transport fault or 5xx; retrying"
            );
            tokio::time::sleep(Duration::from_secs(delay_secs)).await;
            attempt += 1;
        }
    }
}

#[async_trait]
impl ConversationSink for ConversationSourceClient {
    async fn onboard(
        &self,
        request: &ConversationCatalogOnboardRequestV1,
    ) -> Result<ConversationCatalogOnboardResponseV1> {
        let url = self.url("/internal/conversation-source/v1/catalog/onboard");
        self.round_trip("the corpus onboard call", || {
            self.http
                .post(url.clone())
                .bearer_auth(&self.bearer)
                .json(request)
        })
        .await
    }

    async fn post_channels(
        &self,
        request: &ChannelRosterRequestV1,
    ) -> Result<ChannelRosterReceiptV1> {
        let url = self.url("/internal/conversation-source/v1/channels");
        self.round_trip("the corpus roster call", || {
            self.http
                .post(url.clone())
                .bearer_auth(&self.bearer)
                .json(request)
        })
        .await
    }

    async fn cursors(&self, scope: &ConnectorScope) -> Result<ConversationCursorsResponseV1> {
        let url = self.url("/internal/conversation-source/v1/cursors");
        let query = Self::scope_query(scope);
        self.round_trip("the corpus cursors call", || {
            self.http
                .get(url.clone())
                .bearer_auth(&self.bearer)
                .query(&query)
        })
        .await
    }

    async fn post_batch(&self, batch: &ConversationBatchV1) -> Result<ConversationBatchReceiptV1> {
        let url = self.url("/internal/conversation-source/v1/batches");
        self.round_trip("the corpus batch call", || {
            self.http
                .post(url.clone())
                .bearer_auth(&self.bearer)
                .json(batch)
        })
        .await
    }

    async fn post_revisions(
        &self,
        request: &ConversationRevisionsRequestV1,
    ) -> Result<ConversationRevisionsReceiptV1> {
        let url = self.url("/internal/conversation-source/v1/revisions");
        self.round_trip("the corpus revisions call", || {
            self.http
                .post(url.clone())
                .bearer_auth(&self.bearer)
                .json(request)
        })
        .await
    }

    async fn status(&self, scope: &ConnectorScope) -> Result<ConversationStatusResponseV1> {
        let url = self.url("/internal/conversation-source/v1/status");
        let query = Self::scope_query(scope);
        self.round_trip("the corpus status call", || {
            self.http
                .get(url.clone())
                .bearer_auth(&self.bearer)
                .query(&query)
        })
        .await
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
        Err(_) => {
            // The body is not the daemon's error shape (a proxy in the path, a
            // crashed handler). A bounded snippet keeps the diagnosis -- which
            // tier answered, and what it said -- without echoing an unbounded
            // body into logs.
            let snippet = String::from_utf8_lossy(&bytes);
            let snippet: String = snippet.chars().take(256).collect();
            if snippet.trim().is_empty() {
                bail!("conversation-source request failed with {status} and an empty body");
            }
            bail!("conversation-source request failed with {status}: {snippet}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::response::IntoResponse as _;

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

    /// A corpus that answers 503 twice and then succeeds: the call must ride
    /// out the blip rather than costing the whole cycle.
    #[tokio::test]
    async fn a_5xx_blip_is_retried_and_the_call_succeeds() {
        let app = axum::Router::new().route(
            "/internal/conversation-source/v1/cursors",
            axum::routing::get(|| async {
                static FAILURES: std::sync::atomic::AtomicU32 =
                    std::sync::atomic::AtomicU32::new(0);
                let seen = FAILURES.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if seen < 2 {
                    return axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                axum::Json(fixed_cursors()).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut client =
            ConversationSourceClient::new(base_url, "bearer").expect("loopback corpus is safe");
        client.retry_backoff_secs = Vec::new();
        let cursors: ConversationCursorsResponseV1 = client
            .round_trip("the corpus cursors call", || {
                client
                    .http
                    .get(client.url("/internal/conversation-source/v1/cursors"))
                    .bearer_auth(&client.bearer)
            })
            .await
            .expect("two 503s are a blip, not an outage");
        assert_eq!(cursors.channels.len(), 1);
    }

    /// A 4xx is the daemon's answer to this request. Retrying it would burn
    /// the cycle's budget and delay the operator seeing the refusal.
    #[tokio::test]
    async fn a_refusal_is_never_retried() {
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let counter = seen.clone();
        let app = axum::Router::new().route(
            "/internal/conversation-source/v1/cursors",
            axum::routing::get(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    axum::http::StatusCode::FORBIDDEN.into_response()
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut client =
            ConversationSourceClient::new(base_url, "bearer").expect("loopback corpus is safe");
        client.retry_backoff_secs = Vec::new();
        let error = client
            .round_trip::<ConversationCursorsResponseV1>("the corpus cursors call", || {
                client
                    .http
                    .get(client.url("/internal/conversation-source/v1/cursors"))
                    .bearer_auth(&client.bearer)
            })
            .await
            .expect_err("a 403 is an answer");
        assert_eq!(
            seen.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a refusal must be asked exactly once"
        );
        assert!(error.to_string().contains("403"), "{error}");
    }

    /// A non-JSON failure body from something between the producer and the
    /// daemon: the surfaced error keeps a BOUNDED snippet, because that body
    /// is the only diagnosis of which tier answered.
    #[tokio::test]
    async fn a_non_json_failure_body_surfaces_a_bounded_snippet() {
        let app = axum::Router::new().route(
            "/internal/conversation-source/v1/cursors",
            axum::routing::get(|| async {
                (
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("proxy said: {}", "x".repeat(4_096)),
                )
                    .into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let mut client =
            ConversationSourceClient::new(base_url, "bearer").expect("loopback corpus is safe");
        client.retry_backoff_secs = Vec::new();
        let error = client
            .round_trip::<ConversationCursorsResponseV1>("the corpus cursors call", || {
                client
                    .http
                    .get(client.url("/internal/conversation-source/v1/cursors"))
                    .bearer_auth(&client.bearer)
            })
            .await
            .expect_err("502 with a plain body still fails");
        let rendered = error.to_string();
        assert!(rendered.contains("502"), "{rendered}");
        assert!(rendered.contains("proxy said"), "{rendered}");
        assert!(
            rendered.len() < 600,
            "the snippet must be bounded: {rendered}"
        );
    }

    fn fixed_cursors() -> ConversationCursorsResponseV1 {
        ConversationCursorsResponseV1 {
            schema_version: bbox_conversation_source::SCHEMA_VERSION,
            scope: ConnectorScope::try_new("csrc_5f2c1d9a4b6e470e", "slack").unwrap(),
            workspace_id: None,
            channels: vec![bbox_conversation_source::ChannelCursorV1 {
                channel_id: "C0FIXTURE01".to_string(),
                history_watermark: None,
                oldest_observed: None,
                inflight_thread_parents: Vec::new(),
                landed_records: 0,
                revisions: 0,
                tombstones: 0,
                held_revisions: 0,
                last_batch_at: None,
            }],
        }
    }
}

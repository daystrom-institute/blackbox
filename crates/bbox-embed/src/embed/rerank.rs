//! Voyage cross-encoder rerank client (`/v1/rerank`).
//!
//! This is the MODEL rerank stage from the embedding-routing design
//! (Layer 3): after RRF fusion, the fused top-k candidates are re-scored
//! by a hosted cross-encoder. It is opt-in per search call; the heuristic
//! type/temporal multipliers in `bbox_corpus_core::search::rerank` remain
//! the default and the degradation target when this API is unavailable.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const DEFAULT_MODEL: &str = "rerank-2.5-lite";
const DEFAULT_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const FALLBACK_API_KEY_ENV: &str = "DAYSTROM_VOYAGE_API_KEY";
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/rerank";
const DEFAULT_TOP_K: usize = 64;
/// Voyage caps one rerank call at 1,000 documents; the search path sends
/// far fewer, but the client enforces the hard limit.
const MAX_DOCUMENTS: usize = 1_000;

/// `[embed.rerank]` config. Absent config yields the defaults, so the
/// model stage is callable without any embed.toml edits; whether it RUNS
/// is decided per search call, never here.
#[derive(Debug, Clone, Deserialize)]
pub struct RerankConfig {
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    /// How many fused candidates the search path sends to the model.
    #[serde(default = "default_top_k")]
    pub top_k: usize,
}

impl Default for RerankConfig {
    fn default() -> Self {
        Self {
            model: default_model(),
            api_key_env: default_api_key_env(),
            endpoint: default_endpoint(),
            top_k: default_top_k(),
        }
    }
}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.into()
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

fn default_top_k() -> usize {
    DEFAULT_TOP_K
}

#[derive(Debug, Clone, PartialEq)]
pub struct RerankHit {
    /// Index into the submitted documents slice.
    pub index: usize,
    pub relevance_score: f32,
}

pub struct VoyageReranker {
    client: reqwest::Client,
    config: RerankConfig,
    api_key: Option<String>,
}

impl std::fmt::Debug for VoyageReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageReranker")
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl VoyageReranker {
    pub fn from_config(config: RerankConfig) -> Result<Self> {
        let api_key = std::env::var(&config.api_key_env)
            .or_else(|_| {
                if config.api_key_env == DEFAULT_API_KEY_ENV {
                    std::env::var(FALLBACK_API_KEY_ENV)
                } else {
                    Err(std::env::VarError::NotPresent)
                }
            })
            .ok();
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(30))
                .build()
                .context("building voyage rerank HTTP client")?,
            config,
            api_key,
        })
    }

    pub fn config(&self) -> &RerankConfig {
        &self.config
    }

    pub async fn rerank(&self, query: &str, documents: &[String]) -> Result<Vec<RerankHit>> {
        if documents.is_empty() {
            return Ok(Vec::new());
        }
        if documents.len() > MAX_DOCUMENTS {
            bail!(
                "rerank called with {} documents; provider caps a call at {MAX_DOCUMENTS}",
                documents.len()
            );
        }
        let api_key = self
            .api_key
            .as_deref()
            .context("VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required for model rerank")?;
        let raw = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(api_key)
            .json(&RerankRequest {
                model: &self.config.model,
                query,
                documents,
            })
            .send()
            .await
            .context("sending voyage rerank request")?;
        let status = raw.status();
        if !status.is_success() {
            let body = raw.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            bail!(
                "voyage rerank request failed: HTTP {status} documents={} body={snippet}",
                documents.len()
            );
        }
        let response = raw
            .json::<RerankResponse>()
            .await
            .context("decoding voyage rerank response")?;
        let mut hits = Vec::with_capacity(response.data.len());
        for item in response.data {
            if item.index >= documents.len() {
                bail!(
                    "voyage rerank returned index {} beyond the {} submitted documents",
                    item.index,
                    documents.len()
                );
            }
            hits.push(RerankHit {
                index: item.index,
                relevance_score: item.relevance_score,
            });
        }
        Ok(hits)
    }
}

/// Blocking shell for the search path (same contract as
/// `query_cache::embed_query_cached`: callers are already on the blocking
/// pool / sync context). Never retries; a failure degrades the calling
/// search to the heuristic rerank path.
pub fn rerank_blocking(
    config: RerankConfig,
    query: &str,
    documents: &[String],
) -> Result<Vec<RerankHit>> {
    let reranker = VoyageReranker::from_config(config)?;
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            tokio::task::block_in_place(|| handle.block_on(reranker.rerank(query, documents)))
        }
        Err(_) => {
            let runtime = tokio::runtime::Runtime::new().context("creating rerank runtime")?;
            runtime.block_on(reranker.rerank(query, documents))
        }
    }
}

#[derive(Serialize)]
struct RerankRequest<'a> {
    model: &'a str,
    query: &'a str,
    documents: &'a [String],
}

#[derive(Deserialize)]
struct RerankResponse {
    data: Vec<RerankResponseItem>,
}

#[derive(Deserialize)]
struct RerankResponseItem {
    index: usize,
    relevance_score: f32,
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    use super::*;

    fn reranker_for(endpoint: String) -> VoyageReranker {
        VoyageReranker {
            client: reqwest::Client::new(),
            config: RerankConfig {
                endpoint,
                ..RerankConfig::default()
            },
            api_key: Some("test-key".into()),
        }
    }

    #[tokio::test]
    async fn rerank_parses_scores_and_serializes_request_shape() {
        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<serde_json::Value>>>);
        let seen = Seen::default();
        let capture = seen.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/rerank",
                post(move |Json(body): Json<serde_json::Value>| {
                    let capture = capture.clone();
                    async move {
                        capture.0.lock().unwrap().push(body);
                        Json(json!({
                            "data": [
                                {"index": 1, "relevance_score": 0.92},
                                {"index": 0, "relevance_score": 0.15}
                            ]
                        }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let reranker = reranker_for(format!("http://{addr}/v1/rerank"));
        let documents = vec!["first doc".to_string(), "second doc".to_string()];
        let hits = reranker.rerank("the query", &documents).await.unwrap();
        assert_eq!(
            hits,
            vec![
                RerankHit {
                    index: 1,
                    relevance_score: 0.92
                },
                RerankHit {
                    index: 0,
                    relevance_score: 0.15
                },
            ]
        );

        let bodies = seen.0.lock().unwrap().clone();
        assert_eq!(bodies.len(), 1);
        assert_eq!(bodies[0]["model"], DEFAULT_MODEL);
        assert_eq!(bodies[0]["query"], "the query");
        assert_eq!(bodies[0]["documents"][1], "second doc");
    }

    #[tokio::test]
    async fn rerank_rejects_out_of_range_index() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/rerank",
                post(|| async { Json(json!({"data": [{"index": 7, "relevance_score": 0.5}]})) }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let reranker = reranker_for(format!("http://{addr}/v1/rerank"));
        let err = reranker
            .rerank("q", &["only doc".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("beyond"));
    }

    #[tokio::test]
    async fn rerank_empty_documents_short_circuits() {
        let reranker = reranker_for("http://127.0.0.1:9/v1/rerank".into());
        assert!(reranker.rerank("q", &[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn rerank_http_error_is_an_error() {
        use axum::http::StatusCode;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/rerank",
                post(|| async {
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        Json(json!({"detail":"slow"})),
                    )
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let reranker = reranker_for(format!("http://{addr}/v1/rerank"));
        let err = reranker
            .rerank("q", &["doc".to_string()])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 429"));
    }
}

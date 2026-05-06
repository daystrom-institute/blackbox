#![allow(dead_code)] // E1 client is constructed by routing; E2/E3 drive calls.

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::EmbeddingProvider;

pub const VOYAGE_DIMENSIONS: usize = 1024;
const DEFAULT_MODEL: &str = "voyage-code-3";
const DEFAULT_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const FALLBACK_API_KEY_ENV: &str = "DAYSTROM_VOYAGE_API_KEY";
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";

#[derive(Debug, Clone, Deserialize)]
pub struct VoyageConfig {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_model")]
    pub model: String,
    /// Batch-level enforcement lives in the E2 queue layer so retries
    /// and per-route debounce share one throttle point.
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_min: u32,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
}

impl Default for VoyageConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_api_key_env(),
            model: default_model(),
            rate_limit_per_min: default_rate_limit(),
            endpoint: default_endpoint(),
        }
    }
}

fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.into()
}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

fn default_rate_limit() -> u32 {
    2_000
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

pub struct VoyageProvider {
    client: reqwest::Client,
    config: VoyageConfig,
    api_key: Option<String>,
}

impl std::fmt::Debug for VoyageProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageProvider")
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl VoyageProvider {
    pub fn from_config(config: VoyageConfig) -> Result<Self> {
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
                .timeout(Duration::from_secs(60))
                .build()
                .context("building voyage HTTP client")?,
            config,
            api_key,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(endpoint: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .context("building voyage test HTTP client")?,
            config: VoyageConfig {
                endpoint,
                ..VoyageConfig::default()
            },
            api_key: Some("test-key".into()),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageProvider {
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let api_key = self.api_key.as_deref().context(
            "VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required for voyage embeddings",
        )?;
        let raw = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(api_key)
            .json(&VoyageRequest {
                model: &self.config.model,
                input: texts,
            })
            .send()
            .await
            .context("sending voyage embedding request")?;
        let status = raw.status();
        if !status.is_success() {
            let body = raw.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            bail!(
                "voyage embedding request failed: HTTP {status} batch_size={} body={snippet}",
                texts.len()
            );
        }
        let response = raw
            .json::<VoyageResponse>()
            .await
            .context("decoding voyage embedding response")?;
        let vectors = response
            .data
            .into_iter()
            .map(|item| item.embedding)
            .collect::<Vec<_>>();
        for vector in &vectors {
            if vector.len() != VOYAGE_DIMENSIONS {
                bail!(
                    "voyage returned vector with dim {}, expected {VOYAGE_DIMENSIONS}",
                    vector.len()
                );
            }
        }
        Ok(vectors)
    }

    fn dimensions(&self) -> usize {
        VOYAGE_DIMENSIONS
    }

    fn model_name(&self) -> &str {
        &self.config.model
    }

    fn id(&self) -> &str {
        super::VOYAGE_PROVIDER_ID
    }
}

#[derive(Serialize)]
struct VoyageRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct VoyageResponse {
    data: Vec<VoyageEmbedding>,
}

#[derive(Deserialize)]
struct VoyageEmbedding {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use axum::{routing::post, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    async fn voyage_handler() -> Json<serde_json::Value> {
        Json(json!({
            "data": [
                {"embedding": vec![0.25_f32; VOYAGE_DIMENSIONS]}
            ]
        }))
    }

    #[tokio::test]
    async fn voyage_mock_returns_expected_dimensions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/v1/embeddings", post(voyage_handler)),
            )
            .await
            .unwrap();
        });
        let provider = VoyageProvider::for_test(format!("http://{addr}/v1/embeddings")).unwrap();
        assert_eq!(provider.id(), super::super::VOYAGE_PROVIDER_ID);
        assert_eq!(provider.model_name(), DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), VOYAGE_DIMENSIONS);
        let vectors = provider.embed_batch(&["hello".into()]).await.unwrap();
        assert_eq!(vectors[0].len(), VOYAGE_DIMENSIONS);
    }

    #[test]
    fn debug_redacts_api_key() {
        let provider = VoyageProvider::for_test("http://127.0.0.1:9/v1/embeddings".into()).unwrap();
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("test-key"));
    }
}

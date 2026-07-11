#![allow(dead_code)] // E1 client is constructed by routing; E2/E3 drive calls.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{
    EmbedEndpointKind, EmbedInput, EmbedInputType, EmbedOutput, EmbeddingProvider, OutputDType,
    UnsupportedEmbedInput, derive_compatibility_family,
};

pub const OLLAMA_DIMENSIONS: usize = 768;
const DEFAULT_MODEL: &str = "nomic-embed-text";
const DEFAULT_ENDPOINT: &str = "http://localhost:11434";

#[derive(Debug, Clone, Deserialize)]
pub struct OllamaConfig {
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
    #[serde(default = "default_model")]
    pub model: String,
}

impl Default for OllamaConfig {
    fn default() -> Self {
        Self {
            endpoint: default_endpoint(),
            model: default_model(),
        }
    }
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

pub struct OllamaProvider {
    client: reqwest::Client,
    config: OllamaConfig,
    id: String,
}

impl std::fmt::Debug for OllamaProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaProvider")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl OllamaProvider {
    pub fn from_config(id: String, config: &OllamaConfig) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .context("building ollama HTTP client")?,
            config: config.clone(),
            id,
        })
    }

    fn embed_url(&self) -> String {
        format!("{}/api/embed", self.config.endpoint.trim_end_matches('/'))
    }
}

#[async_trait]
impl EmbeddingProvider for OllamaProvider {
    /// Ollama's embed API has no retrieval-role marking; `input_type` is
    /// accepted and ignored.
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        _input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>> {
        let mut texts = Vec::with_capacity(inputs.len());
        for input in inputs {
            match input {
                EmbedInput::Text(text) => texts.push(text.clone()),
                other => {
                    return Err(anyhow::Error::new(UnsupportedEmbedInput {
                        provider_kind: EmbedEndpointKind::Ollama,
                        input_kind: other.kind(),
                    }));
                }
            }
        }
        let response = self
            .client
            .post(self.embed_url())
            .json(&OllamaRequest {
                model: &self.config.model,
                input: &texts,
            })
            .send()
            .await
            .context("sending ollama embedding request")?
            .error_for_status()
            .context("ollama embedding request failed")?
            .json::<OllamaResponse>()
            .await
            .context("decoding ollama embedding response")?;
        let vectors = match (response.embeddings, response.embedding) {
            (Some(vectors), _) => vectors,
            (None, Some(vector)) => vec![vector],
            (None, None) => bail!("ollama embedding response missing embeddings"),
        };
        for vector in &vectors {
            if vector.len() != OLLAMA_DIMENSIONS {
                bail!(
                    "ollama returned vector with dim {}, expected {OLLAMA_DIMENSIONS}",
                    vector.len()
                );
            }
        }
        Ok(vectors.into_iter().map(EmbedOutput::single).collect())
    }

    fn dimensions(&self) -> usize {
        OLLAMA_DIMENSIONS
    }

    fn document_model(&self) -> &str {
        &self.config.model
    }

    fn query_model(&self) -> &str {
        &self.config.model
    }

    fn endpoint_kind(&self) -> EmbedEndpointKind {
        EmbedEndpointKind::Ollama
    }

    fn output_dtype(&self) -> OutputDType {
        OutputDType::Float
    }

    fn compatibility_family(&self) -> String {
        derive_compatibility_family(
            EmbedEndpointKind::Ollama,
            &self.config.model,
            OLLAMA_DIMENSIONS,
            OutputDType::Float,
        )
    }

    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Serialize)]
struct OllamaRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct OllamaResponse {
    embeddings: Option<Vec<Vec<f32>>>,
    embedding: Option<Vec<f32>>,
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use tokio::net::TcpListener;

    use super::*;

    async fn ollama_handler() -> Json<serde_json::Value> {
        Json(json!({
            "embeddings": [
                vec![0.5_f32; OLLAMA_DIMENSIONS]
            ]
        }))
    }

    #[tokio::test]
    async fn ollama_mock_returns_expected_dimensions() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().route("/api/embed", post(ollama_handler)),
            )
            .await
            .unwrap();
        });
        let provider = OllamaProvider::from_config(
            super::super::OLLAMA_PROVIDER_ID.into(),
            &OllamaConfig {
                endpoint: format!("http://{addr}"),
                ..OllamaConfig::default()
            },
        )
        .unwrap();
        assert_eq!(provider.id(), super::super::OLLAMA_PROVIDER_ID);
        assert_eq!(provider.document_model(), DEFAULT_MODEL);
        assert_eq!(provider.query_model(), DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), OLLAMA_DIMENSIONS);
        assert_eq!(
            provider.compatibility_family(),
            "ollama:nomic-embed-text:768:float"
        );
        let outputs = provider
            .embed_batch(
                &[EmbedInput::Text("hello".into())],
                EmbedInputType::Document,
            )
            .await
            .unwrap();
        assert_eq!(outputs[0].vectors[0].len(), OLLAMA_DIMENSIONS);
    }

    #[test]
    fn debug_omits_client_internals() {
        let provider = OllamaProvider::from_config(
            super::super::OLLAMA_PROVIDER_ID.into(),
            &OllamaConfig::default(),
        )
        .unwrap();
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("OllamaProvider"));
        assert!(rendered.contains("config"));
        assert!(!rendered.contains("client"));
    }
}

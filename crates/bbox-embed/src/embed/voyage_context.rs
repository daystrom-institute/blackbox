//! Voyage contextualized chunk embeddings (`/v1/contextualizedembeddings`).
//!
//! Layer 2 of the embedding-routing design: each chunk is encoded in the
//! context of the other chunks of the SAME document, so the endpoint takes
//! document-grouped input (`inputs: [[chunk, ...], ...]`) and returns one
//! vector per chunk. Queries are single-chunk documents (`[[query]]`) with
//! `input_type=query`; their vectors search the same partition. Each exact
//! contextual model is its own compatibility family (no documented
//! cross-version space).

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{
    EmbedEndpointKind, EmbedInput, EmbedInputType, EmbedOutput, EmbeddingProvider, OutputDType,
    UnsupportedEmbedInput,
};

const DEFAULT_MODEL: &str = "voyage-context-4";
const DEFAULT_OUTPUT_DIMENSION: usize = 1024;
const DEFAULT_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const FALLBACK_API_KEY_ENV: &str = "DAYSTROM_VOYAGE_API_KEY";
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/contextualizedembeddings";

/// Config for one `type = "voyage_context"` provider alias. Contextualized
/// routes are always symmetric: the same model encodes document groups and
/// queries (asymmetric pairs have no documented shared space here).
#[derive(Debug, Clone, Deserialize)]
pub struct VoyageContextConfig {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default = "default_model")]
    pub model: String,
    #[serde(default = "default_output_dimension")]
    pub output_dimension: usize,
    #[serde(default)]
    pub output_dtype: OutputDType,
    #[serde(default = "default_rate_limit")]
    pub rate_limit_per_min: u32,
    #[serde(default = "default_endpoint")]
    pub endpoint: String,
}

impl Default for VoyageContextConfig {
    fn default() -> Self {
        Self {
            api_key_env: default_api_key_env(),
            model: default_model(),
            output_dimension: default_output_dimension(),
            output_dtype: OutputDType::Float,
            rate_limit_per_min: default_rate_limit(),
            endpoint: default_endpoint(),
        }
    }
}

impl VoyageContextConfig {
    pub fn validate(&self, alias: &str) -> Result<()> {
        if self.output_dtype != OutputDType::Float {
            bail!(
                "embedding provider `{alias}`: output_dtype `{}` is not supported yet; \
                 only `float` embeddings can be requested",
                self.output_dtype.as_str()
            );
        }
        if !matches!(self.output_dimension, 256 | 512 | 1024 | 2048) {
            bail!(
                "embedding provider `{alias}`: output_dimension {} is not a supported \
                 Voyage output dimension (256, 512, 1024, 2048)",
                self.output_dimension
            );
        }
        Ok(())
    }
}

fn default_api_key_env() -> String {
    DEFAULT_API_KEY_ENV.into()
}

fn default_model() -> String {
    DEFAULT_MODEL.into()
}

fn default_output_dimension() -> usize {
    DEFAULT_OUTPUT_DIMENSION
}

fn default_rate_limit() -> u32 {
    2_000
}

fn default_endpoint() -> String {
    DEFAULT_ENDPOINT.into()
}

pub struct VoyageContextProvider {
    client: reqwest::Client,
    config: VoyageContextConfig,
    id: String,
    api_key: Option<String>,
}

impl std::fmt::Debug for VoyageContextProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageContextProvider")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl VoyageContextProvider {
    pub fn from_config(id: String, config: &VoyageContextConfig) -> Result<Self> {
        config.validate(&id)?;
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
                .timeout(Duration::from_secs(120))
                .build()
                .context("building voyage context HTTP client")?,
            config: config.clone(),
            id,
            api_key,
        })
    }

    #[cfg(test)]
    pub fn for_test(endpoint: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            config: VoyageContextConfig {
                endpoint,
                ..VoyageContextConfig::default()
            },
            id: super::VOYAGE_CONTEXT_PROVIDER_ID.to_string(),
            api_key: Some("test-key".into()),
        })
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageContextProvider {
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>> {
        // Every input becomes one document group: Text is a single-chunk
        // document (the query path), DocumentChunks is the real thing.
        let mut groups: Vec<Vec<String>> = Vec::with_capacity(inputs.len());
        for input in inputs {
            match input {
                EmbedInput::Text(text) => groups.push(vec![text.clone()]),
                EmbedInput::DocumentChunks(chunks) => {
                    if chunks.is_empty() {
                        bail!("contextualized embedding requires at least one chunk per document");
                    }
                    groups.push(chunks.clone());
                }
                other => {
                    return Err(anyhow::Error::new(UnsupportedEmbedInput {
                        provider_kind: EmbedEndpointKind::ContextualizedText,
                        input_kind: other.kind(),
                    }));
                }
            }
        }
        let api_key = self.api_key.as_deref().context(
            "VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required for voyage embeddings",
        )?;
        let raw = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(api_key)
            .json(&ContextRequest {
                model: &self.config.model,
                inputs: &groups,
                input_type: input_type.as_str(),
                output_dimension: self.config.output_dimension,
            })
            .send()
            .await
            .context("sending voyage contextualized embedding request")?;
        let status = raw.status();
        if !status.is_success() {
            let body = raw.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            let message = format!(
                "voyage contextualized embedding request failed: HTTP {status} documents={} body={snippet}",
                groups.len()
            );
            // Same poison contract as the text provider (gap-e3e033ce).
            if status.is_client_error()
                && status != reqwest::StatusCode::REQUEST_TIMEOUT
                && status != reqwest::StatusCode::TOO_MANY_REQUESTS
            {
                return Err(
                    anyhow::Error::new(super::queue::NonRetryableBatchError).context(message)
                );
            }
            bail!("{message}");
        }
        let response = raw
            .json::<ContextResponse>()
            .await
            .context("decoding voyage contextualized embedding response")?;
        if response.data.len() != groups.len() {
            bail!(
                "voyage contextualized response covered {} documents, expected {}",
                response.data.len(),
                groups.len()
            );
        }
        let mut outputs = Vec::with_capacity(groups.len());
        for (document, group) in response.data.into_iter().zip(&groups) {
            if document.data.len() != group.len() {
                bail!(
                    "voyage contextualized response returned {} vectors for a {}-chunk document",
                    document.data.len(),
                    group.len()
                );
            }
            let mut vectors = Vec::with_capacity(document.data.len());
            for item in document.data {
                if item.embedding.len() != self.config.output_dimension {
                    bail!(
                        "voyage returned vector with dim {}, expected {}",
                        item.embedding.len(),
                        self.config.output_dimension
                    );
                }
                vectors.push(item.embedding);
            }
            outputs.push(EmbedOutput { vectors });
        }
        Ok(outputs)
    }

    fn dimensions(&self) -> usize {
        self.config.output_dimension
    }

    fn document_model(&self) -> &str {
        &self.config.model
    }

    fn endpoint_kind(&self) -> EmbedEndpointKind {
        EmbedEndpointKind::ContextualizedText
    }

    fn output_dtype(&self) -> OutputDType {
        self.config.output_dtype
    }

    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Serialize)]
struct ContextRequest<'a> {
    model: &'a str,
    inputs: &'a [Vec<String>],
    input_type: &'a str,
    output_dimension: usize,
}

#[derive(Deserialize)]
struct ContextResponse {
    data: Vec<ContextDocument>,
}

#[derive(Deserialize)]
struct ContextDocument {
    data: Vec<ContextEmbedding>,
}

#[derive(Deserialize)]
struct ContextEmbedding {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    use super::*;

    #[tokio::test]
    async fn document_groups_serialize_nested_and_map_one_vector_per_chunk() {
        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<serde_json::Value>>>);
        let seen = Seen::default();
        let capture = seen.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/contextualizedembeddings",
                post(move |Json(body): Json<serde_json::Value>| {
                    let capture = capture.clone();
                    async move {
                        capture.0.lock().unwrap().push(body.clone());
                        // One embedding per chunk, per document group.
                        let data = body["inputs"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|group| {
                                let chunks = group.as_array().unwrap().len();
                                json!({
                                    "data": (0..chunks)
                                        .map(|_| json!({
                                            "embedding": vec![0.5_f32; DEFAULT_OUTPUT_DIMENSION]
                                        }))
                                        .collect::<Vec<_>>()
                                })
                            })
                            .collect::<Vec<_>>();
                        Json(json!({ "data": data }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let provider =
            VoyageContextProvider::for_test(format!("http://{addr}/v1/contextualizedembeddings"))
                .unwrap();
        assert_eq!(
            provider.endpoint_kind(),
            EmbedEndpointKind::ContextualizedText
        );
        assert_eq!(
            provider.compatibility_family(),
            "voyage-context-4:1024:float"
        );

        let outputs = provider
            .embed_batch(
                &[
                    EmbedInput::DocumentChunks(vec!["c1".into(), "c2".into(), "c3".into()]),
                    EmbedInput::Text("standalone".into()),
                ],
                EmbedInputType::Document,
            )
            .await
            .unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].vectors.len(), 3);
        assert_eq!(outputs[1].vectors.len(), 1);

        let bodies = seen.0.lock().unwrap().clone();
        assert_eq!(bodies[0]["input_type"], "document");
        assert_eq!(bodies[0]["output_dimension"], 1024);
        assert_eq!(bodies[0]["inputs"][0].as_array().unwrap().len(), 3);
        assert_eq!(bodies[0]["inputs"][1][0], "standalone");
    }

    #[tokio::test]
    async fn query_text_is_a_single_chunk_document_with_query_role() {
        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<serde_json::Value>>>);
        let seen = Seen::default();
        let capture = seen.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/contextualizedembeddings",
                post(move |Json(body): Json<serde_json::Value>| {
                    let capture = capture.clone();
                    async move {
                        capture.0.lock().unwrap().push(body);
                        Json(json!({
                            "data": [
                                {"data": [{"embedding": vec![0.5_f32; DEFAULT_OUTPUT_DIMENSION]}]}
                            ]
                        }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let provider =
            VoyageContextProvider::for_test(format!("http://{addr}/v1/contextualizedembeddings"))
                .unwrap();
        let outputs = provider
            .embed_batch(
                &[EmbedInput::Text("find the retry policy".into())],
                EmbedInputType::Query,
            )
            .await
            .unwrap();
        assert_eq!(outputs.len(), 1);
        let vector = outputs.into_iter().next().unwrap().into_single().unwrap();
        assert_eq!(vector.len(), DEFAULT_OUTPUT_DIMENSION);

        let bodies = seen.0.lock().unwrap().clone();
        assert_eq!(bodies[0]["input_type"], "query");
        assert_eq!(bodies[0]["inputs"][0].as_array().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn multimodal_inputs_are_rejected() {
        let provider = VoyageContextProvider::for_test("http://127.0.0.1:9/na".into()).unwrap();
        let err = provider
            .embed_batch(&[EmbedInput::Multimodal(vec![])], EmbedInputType::Document)
            .await
            .unwrap_err();
        assert!(
            err.chain()
                .any(|cause| cause.downcast_ref::<UnsupportedEmbedInput>().is_some())
        );
    }

    #[tokio::test]
    async fn chunk_count_mismatch_is_an_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/contextualizedembeddings",
                post(|| async {
                    Json(json!({
                        "data": [
                            {"data": [{"embedding": vec![0.5_f32; DEFAULT_OUTPUT_DIMENSION]}]}
                        ]
                    }))
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
        let provider =
            VoyageContextProvider::for_test(format!("http://{addr}/v1/contextualizedembeddings"))
                .unwrap();
        let err = provider
            .embed_batch(
                &[EmbedInput::DocumentChunks(vec!["a".into(), "b".into()])],
                EmbedInputType::Document,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("2-chunk document"));
    }
}

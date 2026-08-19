#![allow(dead_code)] // E1 client is constructed by routing; E2/E3 drive calls.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use super::{
    EmbedEndpointKind, EmbedInput, EmbedInputType, EmbedOutput, EmbeddingProvider, OutputDType,
    UnsupportedEmbedInput, derive_compatibility_family,
};

pub const VOYAGE_DIMENSIONS: usize = 1024;
const DEFAULT_MODEL: &str = "voyage-code-3";
const DEFAULT_PROSE_DOCUMENT_MODEL: &str = "voyage-4-large";
const DEFAULT_PROSE_QUERY_MODEL: &str = "voyage-4-lite";
const DEFAULT_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const FALLBACK_API_KEY_ENV: &str = "DAYSTROM_VOYAGE_API_KEY";
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";

/// Config for one `type = "voyage_text"` provider alias. Either `model`
/// (symmetric) or `document_model`/`query_model` (asymmetric within one
/// compatibility family) selects the models; setting both is rejected.
#[derive(Debug, Clone, Deserialize)]
pub struct VoyageConfig {
    #[serde(default = "default_api_key_env")]
    pub api_key_env: String,
    #[serde(default)]
    pub model: Option<String>,
    /// Model used for stored corpus content (one-time indexing cost).
    #[serde(default)]
    pub document_model: Option<String>,
    /// Model used for live queries (per-search cost). Defaults to
    /// `document_model` when only that is set.
    #[serde(default)]
    pub query_model: Option<String>,
    #[serde(default = "default_output_dimension")]
    pub output_dimension: usize,
    #[serde(default)]
    pub output_dtype: OutputDType,
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
            model: Some(DEFAULT_MODEL.into()),
            document_model: None,
            query_model: None,
            output_dimension: default_output_dimension(),
            output_dtype: OutputDType::Float,
            rate_limit_per_min: default_rate_limit(),
            endpoint: default_endpoint(),
        }
    }
}

impl VoyageConfig {
    /// Built-in `voyage_text` alias: voyage-4 prose family, asymmetric
    /// (documents on the large model, queries on the lite model — the
    /// family's shared space is what makes that sound).
    pub fn voyage4_prose_default() -> Self {
        Self {
            model: None,
            document_model: Some(DEFAULT_PROSE_DOCUMENT_MODEL.into()),
            query_model: Some(DEFAULT_PROSE_QUERY_MODEL.into()),
            ..Self::default()
        }
    }

    /// Resolve the (document_model, query_model) pair from the config
    /// shape. Family agreement is enforced by the route loader, not here.
    pub fn resolved_models(&self, alias: &str) -> Result<(String, String)> {
        match (&self.model, &self.document_model, &self.query_model) {
            (Some(_), Some(_), _) | (Some(_), _, Some(_)) => bail!(
                "embedding provider `{alias}`: `model` and `document_model`/`query_model` \
                 are mutually exclusive"
            ),
            (Some(model), None, None) => Ok((model.clone(), model.clone())),
            (None, Some(document), Some(query)) => Ok((document.clone(), query.clone())),
            (None, Some(document), None) => Ok((document.clone(), document.clone())),
            (None, None, Some(_)) => {
                bail!("embedding provider `{alias}`: `query_model` requires `document_model`")
            }
            (None, None, None) => Ok((DEFAULT_MODEL.into(), DEFAULT_MODEL.into())),
        }
    }

    pub fn validate(&self, alias: &str) -> Result<()> {
        if self.output_dtype != OutputDType::Float {
            // Quantized dtypes are part of route/family identity already,
            // but the provider does not decode non-float responses yet.
            // Reject at load rather than storing vectors we cannot read.
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

fn default_output_dimension() -> usize {
    VOYAGE_DIMENSIONS
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
    id: String,
    document_model: String,
    query_model: String,
    api_key: Option<String>,
}

impl std::fmt::Debug for VoyageProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageProvider")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl VoyageProvider {
    pub fn from_config(id: String, config: &VoyageConfig) -> Result<Self> {
        config.validate(&id)?;
        let (document_model, query_model) = config.resolved_models(&id)?;
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
            config: config.clone(),
            id,
            document_model,
            query_model,
            api_key,
        })
    }

    #[cfg(test)]
    pub fn for_test(endpoint: String) -> Result<Self> {
        Self::for_test_with_config(
            endpoint,
            VoyageConfig::default(),
            super::VOYAGE_PROVIDER_ID.to_string(),
        )
    }

    #[cfg(test)]
    pub fn for_test_with_config(
        endpoint: String,
        config: VoyageConfig,
        id: String,
    ) -> Result<Self> {
        let config = VoyageConfig { endpoint, ..config };
        let (document_model, query_model) = config.resolved_models(&id)?;
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .context("building voyage test HTTP client")?,
            config,
            id,
            document_model,
            query_model,
            api_key: Some("test-key".into()),
        })
    }

    fn model_for(&self, input_type: EmbedInputType) -> &str {
        match input_type {
            EmbedInputType::Document => &self.document_model,
            EmbedInputType::Query => &self.query_model,
        }
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageProvider {
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>> {
        let mut texts = Vec::with_capacity(inputs.len());
        for input in inputs {
            match input {
                EmbedInput::Text(text) => texts.push(text.clone()),
                other => {
                    return Err(anyhow::Error::new(UnsupportedEmbedInput {
                        provider_kind: EmbedEndpointKind::Text,
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
            .json(&VoyageRequest {
                model: self.model_for(input_type),
                input: &texts,
                input_type: input_type.as_str(),
                output_dimension: self.config.output_dimension,
            })
            .send()
            .await
            .context("sending voyage embedding request")?;
        let status = raw.status();
        if !status.is_success() {
            let retry_after = super::queue::parse_retry_after_secs(
                raw.headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok()),
            );
            let body = raw.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            let message = format!(
                "voyage embedding request failed: HTTP {status} batch_size={} body={snippet}",
                texts.len()
            );
            // Payload-level 4xx is non-retryable (the queue bisects to the
            // poison item, gap-e3e033ce); 429 carries the rate-limit marker
            // so the queue waits out the per-minute window instead of
            // dropping the batch; everything else is a plain retry.
            return Err(super::queue::classify_provider_http_failure(
                status,
                retry_after,
                message,
            ));
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
            if vector.len() != self.config.output_dimension {
                bail!(
                    "voyage returned vector with dim {}, expected {}",
                    vector.len(),
                    self.config.output_dimension
                );
            }
        }
        Ok(vectors.into_iter().map(EmbedOutput::single).collect())
    }

    fn dimensions(&self) -> usize {
        self.config.output_dimension
    }

    fn document_model(&self) -> &str {
        &self.document_model
    }

    fn query_model(&self) -> &str {
        &self.query_model
    }

    fn endpoint_kind(&self) -> EmbedEndpointKind {
        EmbedEndpointKind::Text
    }

    fn output_dtype(&self) -> OutputDType {
        self.config.output_dtype
    }

    fn compatibility_family(&self) -> String {
        derive_compatibility_family(
            EmbedEndpointKind::Text,
            &self.document_model,
            self.config.output_dimension,
            self.config.output_dtype,
        )
    }

    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Serialize)]
struct VoyageRequest<'a> {
    model: &'a str,
    input: &'a [String],
    input_type: &'a str,
    output_dimension: usize,
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
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    use super::*;

    async fn voyage_handler() -> Json<serde_json::Value> {
        Json(json!({
            "data": [
                {"embedding": vec![0.25_f32; VOYAGE_DIMENSIONS]}
            ]
        }))
    }

    fn text_inputs(texts: &[&str]) -> Vec<EmbedInput> {
        texts
            .iter()
            .map(|text| EmbedInput::Text((*text).to_string()))
            .collect()
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
        assert_eq!(provider.document_model(), DEFAULT_MODEL);
        assert_eq!(provider.query_model(), DEFAULT_MODEL);
        assert_eq!(provider.dimensions(), VOYAGE_DIMENSIONS);
        assert_eq!(provider.compatibility_family(), "voyage-code-3:1024:float");
        let outputs = provider
            .embed_batch(&text_inputs(&["hello"]), EmbedInputType::Document)
            .await
            .unwrap();
        assert_eq!(outputs[0].vectors.len(), 1);
        assert_eq!(outputs[0].vectors[0].len(), VOYAGE_DIMENSIONS);
    }

    /// Documents and queries must reach Voyage role-marked, and an
    /// asymmetric alias must select its model from the input type.
    #[tokio::test]
    async fn document_and_query_serialize_role_and_model() {
        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<serde_json::Value>>>);

        let seen = Seen::default();
        let capture = seen.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/embeddings",
                post(move |Json(body): Json<serde_json::Value>| {
                    let capture = capture.clone();
                    async move {
                        capture.0.lock().unwrap().push(body);
                        voyage_handler().await
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let provider = VoyageProvider::for_test_with_config(
            format!("http://{addr}/v1/embeddings"),
            VoyageConfig::voyage4_prose_default(),
            "voyage_text".into(),
        )
        .unwrap();
        provider
            .embed_batch(&text_inputs(&["stored chunk"]), EmbedInputType::Document)
            .await
            .unwrap();
        provider
            .embed_batch(&text_inputs(&["live query"]), EmbedInputType::Query)
            .await
            .unwrap();

        let bodies = seen.0.lock().unwrap().clone();
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[0]["input_type"], "document");
        assert_eq!(bodies[0]["model"], DEFAULT_PROSE_DOCUMENT_MODEL);
        assert_eq!(bodies[1]["input_type"], "query");
        assert_eq!(bodies[1]["model"], DEFAULT_PROSE_QUERY_MODEL);
        assert_eq!(bodies[0]["output_dimension"], 1024);
    }

    #[tokio::test]
    async fn non_text_inputs_are_rejected_as_non_retryable() {
        let provider = VoyageProvider::for_test("http://127.0.0.1:9/v1/embeddings".into()).unwrap();
        let err = provider
            .embed_batch(
                &[EmbedInput::DocumentChunks(vec!["a".into(), "b".into()])],
                EmbedInputType::Document,
            )
            .await
            .unwrap_err();
        assert!(
            err.chain()
                .any(|cause| cause.downcast_ref::<UnsupportedEmbedInput>().is_some()),
            "expected UnsupportedEmbedInput: {err:#}"
        );
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<super::super::queue::NonRetryableBatchError>()
                .is_some()),
            "unsupported input must be non-retryable: {err:#}"
        );
    }

    /// Payload-level 4xx is non-retryable (queue bisects to the poison
    /// item); 429 stays retryable (backoff is the right response).
    #[tokio::test]
    async fn voyage_400_is_non_retryable_but_429_is_retryable() {
        use axum::http::StatusCode;

        async fn bad_request() -> (StatusCode, Json<serde_json::Value>) {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "Input cannot contain empty strings or empty lists"})),
            )
        }
        async fn rate_limited() -> (StatusCode, Json<serde_json::Value>) {
            (
                StatusCode::TOO_MANY_REQUESTS,
                Json(json!({"detail": "slow down"})),
            )
        }

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new()
                    .route("/bad", post(bad_request))
                    .route("/limit", post(rate_limited)),
            )
            .await
            .unwrap();
        });

        let provider = VoyageProvider::for_test(format!("http://{addr}/bad")).unwrap();
        let err = provider
            .embed_batch(&text_inputs(&[""]), EmbedInputType::Document)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 400"));
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<super::super::queue::NonRetryableBatchError>()
                .is_some()),
            "400 must carry the non-retryable marker: {err:#}"
        );

        let provider = VoyageProvider::for_test(format!("http://{addr}/limit")).unwrap();
        let err = provider
            .embed_batch(&text_inputs(&["ok"]), EmbedInputType::Document)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("HTTP 429"));
        assert!(
            !err.chain().any(|cause| cause
                .downcast_ref::<super::super::queue::NonRetryableBatchError>()
                .is_some()),
            "429 must stay retryable: {err:#}"
        );
    }

    #[test]
    fn model_and_split_models_are_mutually_exclusive() {
        let config = VoyageConfig {
            model: Some("voyage-code-3".into()),
            document_model: Some("voyage-4-large".into()),
            ..VoyageConfig::default()
        };
        let err = config.resolved_models("bad_alias").unwrap_err();
        assert!(err.to_string().contains("mutually exclusive"));
    }

    #[test]
    fn non_float_dtype_is_rejected_for_now() {
        let config = VoyageConfig {
            output_dtype: OutputDType::Int8,
            ..VoyageConfig::default()
        };
        let err = config.validate("voyage").unwrap_err();
        assert!(err.to_string().contains("output_dtype"));
    }

    #[test]
    fn debug_redacts_api_key() {
        let provider = VoyageProvider::for_test("http://127.0.0.1:9/v1/embeddings".into()).unwrap();
        let rendered = format!("{provider:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("test-key"));
    }
}

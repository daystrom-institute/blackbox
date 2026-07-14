//! Voyage multimodal embeddings (`/v1/multimodalembeddings`).
//!
//! Layer 4 of the embedding-routing design: interleaved text/image/video
//! inputs embed into their own compatibility family
//! (`voyage-multimodal-3.5:<dim>:float`) with NO documented sharing with
//! the voyage-4 text space. Two non-negotiables from the design:
//!
//! 1. `truncation` is ALWAYS false. Voyage's default truncation silently
//!    discards an entire image when the cut lands inside it; a dropped
//!    image is not an acceptable silent degradation for visual retrieval.
//! 2. Payload limits are preflighted locally (image <= 20 MB and <= 16M
//!    pixels, video <= 20 MB) so oversized payloads fail as typed poison
//!    before a network round trip, not as an opaque provider 400.
//!
//! Quantized output dtypes are undocumented for the multimodal endpoint;
//! non-float config is rejected until probed.

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use base64::Engine as _;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

use super::{
    EmbedEndpointKind, EmbedInput, EmbedInputType, EmbedOutput, EmbeddingProvider, MultimodalPart,
    OutputDType, UnsupportedEmbedInput,
};

const DEFAULT_MODEL: &str = "voyage-multimodal-3.5";
const DEFAULT_OUTPUT_DIMENSION: usize = 1024;
const DEFAULT_API_KEY_ENV: &str = "VOYAGE_API_KEY";
const FALLBACK_API_KEY_ENV: &str = "DAYSTROM_VOYAGE_API_KEY";
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/multimodalembeddings";

// pub(crate): shared with `visual_normalize`, which normalizes payloads
// against these same limits before they ever reach `preflight_part`.
pub(crate) const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
pub(crate) const MAX_IMAGE_PIXELS: u64 = 16_000_000;
const MAX_VIDEO_BYTES: usize = 20 * 1024 * 1024;
const MAX_INPUTS_PER_REQUEST: usize = 1_000;

/// Max permitted long-side:short-side ratio. Not documented: the provider's
/// published image constraints (16M pixels, 20MB) say nothing about aspect
/// ratio, and the observed HTTP 400 body ("image aspect ratio is not within
/// the permitted range") was truncated before the actual number. Checked
/// docs.voyageai.com/docs/multimodal-embeddings and
/// docs.voyageai.com/reference/multimodal-embeddings-api directly
/// (2026-07-13): neither documents a ratio limit. Using a conservative
/// 19:1 ceiling until Voyage documents (or support discloses) the real
/// value — comfortably clears realistic thin-strip OCR scan pages while
/// staying well inside any range a patch-based vision encoder would
/// plausibly enforce.
pub(crate) const MAX_ASPECT_RATIO: f64 = 19.0;

/// Config for one `type = "voyage_multimodal"` provider alias.
#[derive(Debug, Clone, Deserialize)]
pub struct VoyageMultimodalConfig {
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

impl Default for VoyageMultimodalConfig {
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

impl VoyageMultimodalConfig {
    pub fn validate(&self, alias: &str) -> Result<()> {
        if self.output_dtype != OutputDType::Float {
            // The text endpoints document quantized dtypes; the multimodal
            // guide does not. Treat quantization as unavailable until a
            // probe proves otherwise (design: External Model Facts).
            bail!(
                "embedding provider `{alias}`: output_dtype `{}` is not supported for \
                 multimodal embeddings (undocumented by the provider); use `float`",
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

/// Local payload guard, run before any bytes leave the host. Violations
/// are poison (identical on retry), sourced from `UnsupportedEmbedInput`'s
/// non-retryable contract via a dedicated typed error.
#[derive(Debug, Clone)]
pub struct MultimodalPayloadError {
    pub reason: String,
}

impl std::fmt::Display for MultimodalPayloadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "multimodal payload rejected: {}", self.reason)
    }
}

impl std::error::Error for MultimodalPayloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        static MARKER: super::queue::NonRetryableBatchError = super::queue::NonRetryableBatchError;
        Some(&MARKER)
    }
}

pub fn preflight_part(part: &MultimodalPart) -> Result<()> {
    match part {
        MultimodalPart::Text(_) => Ok(()),
        MultimodalPart::ImageBytes { mime, bytes } => {
            if bytes.len() > MAX_IMAGE_BYTES {
                return Err(anyhow::Error::new(MultimodalPayloadError {
                    reason: format!(
                        "image ({mime}) is {} bytes; provider cap is {MAX_IMAGE_BYTES}",
                        bytes.len()
                    ),
                }));
            }
            match imagesize::blob_size(bytes) {
                Ok(size) => {
                    let pixels = size.width as u64 * size.height as u64;
                    if pixels > MAX_IMAGE_PIXELS {
                        return Err(anyhow::Error::new(MultimodalPayloadError {
                            reason: format!(
                                "image ({mime}) is {}x{} = {pixels} pixels; provider cap \
                                 is {MAX_IMAGE_PIXELS}",
                                size.width, size.height
                            ),
                        }));
                    }
                    let (w, h) = (size.width.max(1) as f64, size.height.max(1) as f64);
                    let ratio = w.max(h) / w.min(h);
                    if ratio > MAX_ASPECT_RATIO {
                        return Err(anyhow::Error::new(MultimodalPayloadError {
                            reason: format!(
                                "image ({mime}) is {}x{} (aspect ratio {ratio:.1}:1); \
                                 local cap is {MAX_ASPECT_RATIO}:1 (see MAX_ASPECT_RATIO doc comment)",
                                size.width, size.height
                            ),
                        }));
                    }
                    Ok(())
                }
                Err(err) => Err(anyhow::Error::new(MultimodalPayloadError {
                    reason: format!(
                        "image ({mime}) dimensions could not be read ({err}); refusing to \
                         send an unverifiable payload"
                    ),
                })),
            }
        }
        MultimodalPart::VideoBytes { mime, bytes } => {
            if bytes.len() > MAX_VIDEO_BYTES {
                return Err(anyhow::Error::new(MultimodalPayloadError {
                    reason: format!(
                        "video ({mime}) is {} bytes; provider cap is {MAX_VIDEO_BYTES}",
                        bytes.len()
                    ),
                }));
            }
            Ok(())
        }
    }
}

pub struct VoyageMultimodalProvider {
    client: reqwest::Client,
    config: VoyageMultimodalConfig,
    id: String,
    api_key: Option<String>,
}

impl std::fmt::Debug for VoyageMultimodalProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageMultimodalProvider")
            .field("id", &self.id)
            .field("config", &self.config)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl VoyageMultimodalProvider {
    pub fn from_config(id: String, config: &VoyageMultimodalConfig) -> Result<Self> {
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
                .context("building voyage multimodal HTTP client")?,
            config: config.clone(),
            id,
            api_key,
        })
    }

    #[cfg(test)]
    pub fn for_test(endpoint: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::new(),
            config: VoyageMultimodalConfig {
                endpoint,
                ..VoyageMultimodalConfig::default()
            },
            id: super::VOYAGE_VISUAL_PROVIDER_ID.to_string(),
            api_key: Some("test-key".into()),
        })
    }

    fn content_item(part: &MultimodalPart) -> serde_json::Value {
        match part {
            MultimodalPart::Text(text) => json!({"type": "text", "text": text}),
            MultimodalPart::ImageBytes { mime, bytes } => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                json!({
                    "type": "image_base64",
                    "image_base64": format!("data:{mime};base64,{encoded}"),
                })
            }
            MultimodalPart::VideoBytes { mime, bytes } => {
                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                json!({
                    "type": "video_base64",
                    "video_base64": format!("data:{mime};base64,{encoded}"),
                })
            }
        }
    }
}

#[async_trait]
impl EmbeddingProvider for VoyageMultimodalProvider {
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>> {
        if inputs.len() > MAX_INPUTS_PER_REQUEST {
            bail!(
                "multimodal embedding called with {} inputs; provider caps a request at \
                 {MAX_INPUTS_PER_REQUEST}",
                inputs.len()
            );
        }
        let mut request_inputs = Vec::with_capacity(inputs.len());
        for input in inputs {
            let content = match input {
                // Text is accepted (the model embeds text into the same
                // space), but text-only buckets must not route here by
                // default — that is a route-config decision, not a
                // provider-seam rejection.
                EmbedInput::Text(text) => {
                    vec![Self::content_item(&MultimodalPart::Text(text.clone()))]
                }
                EmbedInput::Multimodal(parts) => {
                    if parts.is_empty() {
                        bail!("multimodal embedding requires at least one content part");
                    }
                    for part in parts {
                        preflight_part(part)?;
                    }
                    parts.iter().map(Self::content_item).collect()
                }
                other => {
                    return Err(anyhow::Error::new(UnsupportedEmbedInput {
                        provider_kind: EmbedEndpointKind::Multimodal,
                        input_kind: other.kind(),
                    }));
                }
            };
            request_inputs.push(json!({"content": content}));
        }
        let api_key = self.api_key.as_deref().context(
            "VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required for voyage embeddings",
        )?;
        let raw = self
            .client
            .post(&self.config.endpoint)
            .bearer_auth(api_key)
            .json(&json!({
                "model": self.config.model,
                "inputs": request_inputs,
                "input_type": input_type.as_str(),
                "output_dimension": self.config.output_dimension,
                // Never let the provider silently drop an image mid-input.
                "truncation": false,
            }))
            .send()
            .await
            .context("sending voyage multimodal embedding request")?;
        let status = raw.status();
        if !status.is_success() {
            let body = raw.text().await.unwrap_or_default();
            let snippet: String = body.chars().take(512).collect();
            let message = format!(
                "voyage multimodal embedding request failed: HTTP {status} inputs={} body={snippet}",
                inputs.len()
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
            .json::<MultimodalResponse>()
            .await
            .context("decoding voyage multimodal embedding response")?;
        if response.data.len() != inputs.len() {
            bail!(
                "voyage multimodal response covered {} inputs, expected {}",
                response.data.len(),
                inputs.len()
            );
        }
        let mut outputs = Vec::with_capacity(response.data.len());
        for item in response.data {
            if item.embedding.len() != self.config.output_dimension {
                bail!(
                    "voyage returned vector with dim {}, expected {}",
                    item.embedding.len(),
                    self.config.output_dimension
                );
            }
            outputs.push(EmbedOutput::single(item.embedding));
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
        EmbedEndpointKind::Multimodal
    }

    fn output_dtype(&self) -> OutputDType {
        self.config.output_dtype
    }

    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(Deserialize)]
struct MultimodalResponse {
    data: Vec<MultimodalEmbedding>,
}

#[derive(Deserialize)]
struct MultimodalEmbedding {
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests {
    use axum::{Json, Router, routing::post};
    use serde_json::json;
    use std::sync::{Arc, Mutex};
    use tokio::net::TcpListener;

    use super::*;

    /// Minimal valid 1x1 PNG (67 bytes) for guard and serialization tests.
    fn tiny_png() -> Vec<u8> {
        const PNG_1X1: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00,
            0x00, 0x1F, 0x15, 0xC4, 0x89, 0x00, 0x00, 0x00, 0x0A, 0x49, 0x44, 0x41, 0x54, 0x78,
            0x9C, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0D, 0x0A, 0x2D, 0xB4, 0x00,
            0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        PNG_1X1.to_vec()
    }

    /// PNG header claiming 8000x8000 (64M pixels) with no real body — the
    /// guard reads dimensions from the header, so this is enough to trip
    /// the pixel cap without a 60MB fixture.
    fn huge_png_header() -> Vec<u8> {
        let mut bytes = tiny_png();
        // IHDR width/height live at fixed offsets 16..24.
        bytes[16..20].copy_from_slice(&8_000u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&8_000u32.to_be_bytes());
        bytes
    }

    /// PNG header claiming a 40x2000 thin scan strip (20:1, over
    /// `MAX_ASPECT_RATIO`) with no real body, same header-only trick as
    /// `huge_png_header`.
    fn thin_strip_png_header() -> Vec<u8> {
        let mut bytes = tiny_png();
        bytes[16..20].copy_from_slice(&40u32.to_be_bytes());
        bytes[20..24].copy_from_slice(&2_000u32.to_be_bytes());
        bytes
    }

    #[test]
    fn preflight_accepts_small_image_and_rejects_pixel_bomb() {
        preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: tiny_png(),
        })
        .unwrap();

        let err = preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: huge_png_header(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("pixels"));
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<super::super::queue::NonRetryableBatchError>()
                .is_some()),
            "payload violations must be poison: {err:#}"
        );
    }

    #[test]
    fn preflight_rejects_aspect_ratio_violation_as_poison() {
        // Conformant (1:1) still accepted alongside the ratio check.
        preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: tiny_png(),
        })
        .unwrap();

        let err = preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: thin_strip_png_header(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("aspect ratio"));
        assert!(
            err.chain().any(|cause| cause
                .downcast_ref::<super::super::queue::NonRetryableBatchError>()
                .is_some()),
            "aspect ratio violations must be poison: {err:#}"
        );
    }

    #[test]
    fn preflight_rejects_undecodable_image_and_oversized_video() {
        let err = preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: vec![0xFF; 32],
        })
        .unwrap_err();
        assert!(err.to_string().contains("could not be read"));

        let err = preflight_part(&MultimodalPart::VideoBytes {
            mime: "video/mp4".into(),
            bytes: vec![0; MAX_VIDEO_BYTES + 1],
        })
        .unwrap_err();
        assert!(err.to_string().contains("provider cap"));
    }

    #[tokio::test]
    async fn interleaved_parts_serialize_with_truncation_off() {
        #[derive(Clone, Default)]
        struct Seen(Arc<Mutex<Vec<serde_json::Value>>>);
        let seen = Seen::default();
        let capture = seen.clone();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let app = Router::new().route(
                "/v1/multimodalembeddings",
                post(move |Json(body): Json<serde_json::Value>| {
                    let capture = capture.clone();
                    async move {
                        let inputs = body["inputs"].as_array().unwrap().len();
                        capture.0.lock().unwrap().push(body);
                        Json(json!({
                            "data": (0..inputs)
                                .map(|_| json!({
                                    "embedding": vec![0.5_f32; DEFAULT_OUTPUT_DIMENSION]
                                }))
                                .collect::<Vec<_>>()
                        }))
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });

        let provider =
            VoyageMultimodalProvider::for_test(format!("http://{addr}/v1/multimodalembeddings"))
                .unwrap();
        assert_eq!(
            provider.compatibility_family(),
            "voyage-multimodal-3.5:1024:float"
        );

        let outputs = provider
            .embed_batch(
                &[
                    EmbedInput::Multimodal(vec![
                        MultimodalPart::Text("figure 3 caption".into()),
                        MultimodalPart::ImageBytes {
                            mime: "image/png".into(),
                            bytes: tiny_png(),
                        },
                    ]),
                    EmbedInput::Text("text-only query".into()),
                ],
                EmbedInputType::Document,
            )
            .await
            .unwrap();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs[0].vectors[0].len(), DEFAULT_OUTPUT_DIMENSION);

        let bodies = seen.0.lock().unwrap().clone();
        let body = &bodies[0];
        assert_eq!(body["truncation"], false);
        assert_eq!(body["input_type"], "document");
        assert_eq!(body["output_dimension"], 1024);
        let content = body["inputs"][0]["content"].as_array().unwrap();
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "image_base64");
        assert!(
            content[1]["image_base64"]
                .as_str()
                .unwrap()
                .starts_with("data:image/png;base64,")
        );
        assert_eq!(body["inputs"][1]["content"][0]["type"], "text");
    }

    #[tokio::test]
    async fn document_chunks_are_rejected() {
        let provider = VoyageMultimodalProvider::for_test("http://127.0.0.1:9/na".into()).unwrap();
        let err = provider
            .embed_batch(
                &[EmbedInput::DocumentChunks(vec!["a".into()])],
                EmbedInputType::Document,
            )
            .await
            .unwrap_err();
        assert!(
            err.chain()
                .any(|cause| cause.downcast_ref::<UnsupportedEmbedInput>().is_some())
        );
    }

    #[test]
    fn non_float_dtype_is_rejected() {
        let config = VoyageMultimodalConfig {
            output_dtype: OutputDType::Int8,
            ..VoyageMultimodalConfig::default()
        };
        assert!(config.validate("voyage_visual").is_err());
    }
}

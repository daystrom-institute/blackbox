#![allow(dead_code)] // E1 lands provider/routing surface; E2/E3 wire live consumers.

pub mod ollama;
pub mod query_cache;
pub mod queue;
pub mod rerank;
pub mod visual_normalize;
pub mod voyage;
pub mod voyage_context;
pub mod voyage_multimodal;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use rmcp::schemars;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_corpus_core::entity_ref::EntityRef;

pub const VOYAGE_PROVIDER_ID: &str = "voyage";
pub const OLLAMA_PROVIDER_ID: &str = "ollama";
pub const VOYAGE_CODE_PROVIDER_ID: &str = "voyage_code";
pub const VOYAGE_TEXT_PROVIDER_ID: &str = "voyage_text";
pub const VOYAGE_CONTEXT_PROVIDER_ID: &str = "voyage_context";
pub const VOYAGE_VISUAL_PROVIDER_ID: &str = "voyage_visual";

/// Retrieval role of an embed call. Stored corpus content is `Document`;
/// live search strings are `Query`. Providers that support role-marked
/// embeddings (Voyage `input_type`) send it; others ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedInputType {
    Query,
    Document,
}

impl EmbedInputType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Query => "query",
            Self::Document => "document",
        }
    }
}

/// One embeddable unit. Text-only providers accept `Text`; contextualized
/// providers (Layer 2) additionally accept `DocumentChunks`; multimodal
/// providers (Layer 4) accept `Multimodal`. Declared up front so the queue
/// and provider seams don't churn when those layers land.
#[derive(Debug, Clone, PartialEq)]
pub enum EmbedInput {
    Text(String),
    /// Ordered chunks of one document, embedded together so each chunk's
    /// vector carries document context. Never split across batches.
    DocumentChunks(Vec<String>),
    Multimodal(Vec<MultimodalPart>),
}

impl EmbedInput {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Text(_) => "text",
            Self::DocumentChunks(_) => "document_chunks",
            Self::Multimodal(_) => "multimodal",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum MultimodalPart {
    Text(String),
    ImageBytes { mime: String, bytes: Vec<u8> },
    VideoBytes { mime: String, bytes: Vec<u8> },
}

/// Vectors produced for one `EmbedInput`. `Text` yields exactly one;
/// `DocumentChunks` yields one per chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbedOutput {
    pub vectors: Vec<Vec<f32>>,
}

impl EmbedOutput {
    pub fn single(vector: Vec<f32>) -> Self {
        Self {
            vectors: vec![vector],
        }
    }

    pub fn into_single(self) -> Result<Vec<f32>> {
        let mut vectors = self.vectors;
        match vectors.len() {
            1 => Ok(vectors.remove(0)),
            n => bail!("expected exactly one vector for a text input, got {n}"),
        }
    }
}

/// Typed marker for an input shape the provider does not support (e.g.
/// `Multimodal` sent to a text provider). Routing to the wrong endpoint
/// kind fails identically on retry, so the queue must treat it as poison.
#[derive(Debug, Clone, Copy)]
pub struct UnsupportedEmbedInput {
    pub provider_kind: EmbedEndpointKind,
    pub input_kind: &'static str,
}

impl std::fmt::Display for UnsupportedEmbedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} embedding provider does not support {} inputs",
            self.provider_kind, self.input_kind
        )
    }
}

impl std::error::Error for UnsupportedEmbedInput {
    /// Sourced from the queue's non-retryable marker: routing an input
    /// shape to the wrong provider fails identically on retry, so the
    /// queue must treat it as poison rather than backing off.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        static MARKER: queue::NonRetryableBatchError = queue::NonRetryableBatchError;
        Some(&MARKER)
    }
}

/// Which provider API family a route talks to. Distinct from the
/// compatibility family: two endpoint kinds never share a vector space,
/// but one endpoint kind can host several incompatible families.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EmbedEndpointKind {
    Text,
    ContextualizedText,
    Multimodal,
    Ollama,
}

/// Stored vector element encoding. Part of compatibility-family identity:
/// binary-quantized vectors are not comparable with float vectors of the
/// same model and dimension.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OutputDType {
    #[default]
    Float,
    Int8,
    Uint8,
    Binary,
    Ubinary,
}

impl OutputDType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Int8 => "int8",
            Self::Uint8 => "uint8",
            Self::Binary => "binary",
            Self::Ubinary => "ubinary",
        }
    }
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Embed a batch once. Transient errors propagate to the caller;
    /// retry policy and exponential backoff are owned by the E2 queue.
    /// Unsupported input shapes fail with `UnsupportedEmbedInput` in the
    /// error chain (poison, never retryable).
    async fn embed_batch(
        &self,
        inputs: &[EmbedInput],
        input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>>;
    fn dimensions(&self) -> usize;
    /// Model used for `Document` inputs.
    fn document_model(&self) -> &str;
    /// Model used for `Query` inputs; equals `document_model()` unless the
    /// route is asymmetric within one compatibility family.
    fn query_model(&self) -> &str {
        self.document_model()
    }
    fn endpoint_kind(&self) -> EmbedEndpointKind;
    fn output_dtype(&self) -> OutputDType {
        OutputDType::Float
    }
    fn compatibility_family(&self) -> String {
        derive_compatibility_family(
            self.endpoint_kind(),
            self.document_model(),
            self.dimensions(),
            self.output_dtype(),
        )
    }
    fn id(&self) -> &str;
}

/// Code-owned compatibility-family derivation (never operator-supplied).
/// Family identity = model family + dimension + dtype; provider aliases and
/// hosting (hosted voyage-4-nano vs local open-weight) do not change it.
/// Unknown models default to exact-model families until classified.
pub fn derive_compatibility_family(
    endpoint_kind: EmbedEndpointKind,
    model: &str,
    dimensions: usize,
    dtype: OutputDType,
) -> String {
    let dtype = dtype.as_str();
    if endpoint_kind == EmbedEndpointKind::Ollama {
        return format!("ollama:{model}:{dimensions}:{dtype}");
    }
    // The voyage-4 text family shares one embedding space across
    // large/base/lite/nano; every other model is its own space. Never
    // inferred from equal dimensions.
    let family_model = if model == "voyage-4" || model.starts_with("voyage-4-") {
        "voyage-4"
    } else {
        model
    };
    format!("{family_model}:{dimensions}:{dtype}")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bucket {
    Knowledge,
    Code,
    Docs,
    Transcripts,
    GitMessage,
    Notes,
    Threads,
    AgentManifest,
}

impl Bucket {
    pub const ALL: [Bucket; 8] = [
        Bucket::Knowledge,
        Bucket::Code,
        Bucket::Docs,
        Bucket::Transcripts,
        Bucket::GitMessage,
        Bucket::Notes,
        Bucket::Threads,
        Bucket::AgentManifest,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Knowledge => "knowledge",
            Self::Code => "code",
            Self::Docs => "docs",
            Self::Transcripts => "transcripts",
            Self::GitMessage => "git_message",
            Self::Notes => "notes",
            Self::Threads => "threads",
            Self::AgentManifest => "agent_manifest",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    pub bucket: Bucket,
    pub project_id: Option<String>,
    pub provider_id: String,
    pub endpoint_kind: EmbedEndpointKind,
    pub document_model: String,
    /// Equals `document_model` unless the route is asymmetric; asymmetric
    /// pairs must share a compatibility family (enforced at config load).
    pub query_model: String,
    pub dimensions: usize,
    pub output_dtype: OutputDType,
    pub compatibility_family: String,
}

impl Route {
    /// Exact partition identity. Partitions are keyed by what produced the
    /// stored vectors: provider alias + document model + dimension + dtype.
    /// Float dtype hashes byte-identically to the pre-dtype algorithm so
    /// existing partitions are not orphaned by this metadata change; any
    /// non-float dtype forces a new partition.
    pub fn vector_route_id(&self) -> String {
        vector_route_id_for(
            &self.provider_id,
            &self.document_model,
            self.dimensions,
            self.output_dtype,
        )
    }
}

/// The partition-id hashing logic `Route::vector_route_id` delegates to,
/// extracted so visual routing (chunk-kind-keyed, not `Bucket`-keyed, see
/// `EmbeddingRouter::visual_route`) can derive the same identity from a
/// resolved `&dyn EmbeddingProvider` without needing a `Route`/`Bucket`.
pub fn vector_route_id_for(
    provider_id: &str,
    document_model: &str,
    dimensions: usize,
    output_dtype: OutputDType,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(provider_id.as_bytes());
    hasher.update([0]);
    hasher.update(document_model.as_bytes());
    hasher.update([0]);
    hasher.update(dimensions.to_string().as_bytes());
    if output_dtype != OutputDType::Float {
        hasher.update([0]);
        hasher.update(output_dtype.as_str().as_bytes());
    }
    let digest = hasher.finalize();
    let dtype_component = if output_dtype == OutputDType::Float {
        String::new()
    } else {
        format!("{}-", output_dtype.as_str())
    };
    format!(
        "{}-{}-{}-{}{:02x}{:02x}{:02x}{:02x}",
        sanitize_route_component(provider_id),
        sanitize_route_component(document_model),
        dimensions,
        dtype_component,
        digest[0],
        digest[1],
        digest[2],
        digest[3]
    )
}

/// Route metadata for one visual chunk kind: the visual-routing analog of
/// [`Route`], minus `bucket`/`project_id`/`query_model` (visual routes are
/// keyed by chunk kind string, never `Bucket`, and are always symmetric:
/// `voyage-multimodal-3.5` has no asymmetric document/query model story).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRouteMeta {
    pub provider_id: String,
    pub document_model: String,
    pub dimensions: usize,
    pub output_dtype: OutputDType,
    pub compatibility_family: String,
}

impl VisualRouteMeta {
    pub fn vector_route_id(&self) -> String {
        vector_route_id_for(
            &self.provider_id,
            &self.document_model,
            self.dimensions,
            self.output_dtype,
        )
    }
}

fn sanitize_route_component(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmbedConfigFile {
    #[serde(default)]
    pub embed: EmbedConfig,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct EmbedConfig {
    #[serde(default)]
    pub providers: ProviderConfigs,
    #[serde(default)]
    pub routes: RoutesConfig,
    /// `[embed.rerank]` — model rerank stage config. Absent means
    /// defaults (rerank-2.5-lite); the stage only runs when a search
    /// call opts in.
    #[serde(default)]
    pub rerank: Option<rerank::RerankConfig>,
}

/// Typed provider config for one alias. The alias (table key) is the
/// route-facing provider id; `type` selects the implementation. Multiple
/// aliases of the same type with different models are the point.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ProviderAliasConfig {
    VoyageText(voyage::VoyageConfig),
    VoyageContext(voyage_context::VoyageContextConfig),
    VoyageMultimodal(voyage_multimodal::VoyageMultimodalConfig),
    Ollama(ollama::OllamaConfig),
}

/// Provider alias map. Legacy `[embed.providers.voyage]` and
/// `[embed.providers.ollama]` tables (no `type` field) still parse as their
/// historical shapes; any other alias must carry an explicit `type`.
/// Built-in aliases (`voyage`, `voyage_code`, `voyage_text`, `ollama`) are
/// synthesized at lookup when the config does not define them.
#[derive(Debug, Clone, Default)]
pub struct ProviderConfigs {
    aliases: BTreeMap<String, ProviderAliasConfig>,
}

impl ProviderConfigs {
    pub fn get(&self, alias: &str) -> Option<ProviderAliasConfig> {
        if let Some(config) = self.aliases.get(alias) {
            return Some(config.clone());
        }
        builtin_alias(alias)
    }

    #[cfg(test)]
    fn insert(&mut self, alias: impl Into<String>, config: ProviderAliasConfig) {
        self.aliases.insert(alias.into(), config);
    }
}

fn builtin_alias(alias: &str) -> Option<ProviderAliasConfig> {
    match alias {
        // `voyage` is the legacy default alias every existing partition id
        // is keyed under; `voyage_code` is its explicit modern name.
        VOYAGE_PROVIDER_ID | VOYAGE_CODE_PROVIDER_ID => Some(ProviderAliasConfig::VoyageText(
            voyage::VoyageConfig::default(),
        )),
        VOYAGE_TEXT_PROVIDER_ID => Some(ProviderAliasConfig::VoyageText(
            voyage::VoyageConfig::voyage4_prose_default(),
        )),
        VOYAGE_CONTEXT_PROVIDER_ID => Some(ProviderAliasConfig::VoyageContext(
            voyage_context::VoyageContextConfig::default(),
        )),
        VOYAGE_VISUAL_PROVIDER_ID => Some(ProviderAliasConfig::VoyageMultimodal(
            voyage_multimodal::VoyageMultimodalConfig::default(),
        )),
        OLLAMA_PROVIDER_ID => Some(ProviderAliasConfig::Ollama(ollama::OllamaConfig::default())),
        _ => None,
    }
}

impl<'de> Deserialize<'de> for ProviderConfigs {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error as _;
        let raw = BTreeMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut aliases = BTreeMap::new();
        for (alias, value) in raw {
            let config = if value.get("type").is_some() {
                value
                    .try_into::<ProviderAliasConfig>()
                    .map_err(|err| D::Error::custom(format!("provider alias `{alias}`: {err}")))?
            } else if alias == VOYAGE_PROVIDER_ID {
                value
                    .try_into::<voyage::VoyageConfig>()
                    .map(ProviderAliasConfig::VoyageText)
                    .map_err(|err| D::Error::custom(format!("provider alias `{alias}`: {err}")))?
            } else if alias == OLLAMA_PROVIDER_ID {
                value
                    .try_into::<ollama::OllamaConfig>()
                    .map(ProviderAliasConfig::Ollama)
                    .map_err(|err| D::Error::custom(format!("provider alias `{alias}`: {err}")))?
            } else {
                return Err(D::Error::custom(format!(
                    "embedding provider alias `{alias}` is missing required field `type`"
                )));
            };
            aliases.insert(alias, config);
        }
        Ok(Self { aliases })
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RoutesConfig {
    pub knowledge: Option<String>,
    pub code: Option<String>,
    pub docs: Option<String>,
    pub transcripts: Option<String>,
    pub git_message: Option<String>,
    pub notes: Option<String>,
    pub threads: Option<String>,
    pub agent_manifest: Option<String>,
    #[serde(default)]
    pub per_project: BTreeMap<String, BucketRoutes>,
    /// `[embed.routes.visual]` — visual chunk kind (`pdf_figure`,
    /// `slide_image`, ...) to provider alias. Visual retrieval is a
    /// separate route family: these must reference multimodal-typed
    /// aliases, and no text bucket ever falls back here.
    #[serde(default)]
    pub visual: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct BucketRoutes {
    pub knowledge: Option<String>,
    pub code: Option<String>,
    pub docs: Option<String>,
    pub transcripts: Option<String>,
    pub git_message: Option<String>,
    pub notes: Option<String>,
    pub threads: Option<String>,
    pub agent_manifest: Option<String>,
}

impl BucketRoutes {
    fn get(&self, bucket: Bucket) -> Option<&str> {
        match bucket {
            Bucket::Knowledge => self.knowledge.as_deref(),
            Bucket::Code => self.code.as_deref(),
            Bucket::Docs => self.docs.as_deref(),
            Bucket::Transcripts => self.transcripts.as_deref(),
            Bucket::GitMessage => self.git_message.as_deref(),
            Bucket::Notes => self.notes.as_deref(),
            Bucket::Threads => self.threads.as_deref(),
            Bucket::AgentManifest => self.agent_manifest.as_deref(),
        }
    }
}

impl RoutesConfig {
    fn global(&self, bucket: Bucket) -> Option<&str> {
        match bucket {
            Bucket::Knowledge => self.knowledge.as_deref(),
            Bucket::Code => self.code.as_deref(),
            Bucket::Docs => self.docs.as_deref(),
            Bucket::Transcripts => self.transcripts.as_deref(),
            Bucket::GitMessage => self.git_message.as_deref(),
            Bucket::Notes => self.notes.as_deref(),
            Bucket::Threads => self.threads.as_deref(),
            Bucket::AgentManifest => self.agent_manifest.as_deref(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct EmbeddingRouter {
    config: EmbedConfig,
}

/// Process-wide test override for `load_default()`. Tests that install a
/// per-test vector store must also pin the router: route derivation
/// otherwise reads the HOST's real embed.toml and the derived partition
/// ids drift with operator config (the same isolation invariant as
/// `bbox_vectors::install_test_global`).
#[cfg(any(test, feature = "test-support"))]
fn test_router_slot() -> &'static parking_lot::RwLock<Option<EmbeddingRouter>> {
    static SLOT: std::sync::OnceLock<parking_lot::RwLock<Option<EmbeddingRouter>>> =
        std::sync::OnceLock::new();
    SLOT.get_or_init(|| parking_lot::RwLock::new(None))
}

#[cfg(any(test, feature = "test-support"))]
pub struct TestRouterGuard;

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestRouterGuard {
    fn drop(&mut self) {
        *test_router_slot().write() = None;
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn install_test_router(router: EmbeddingRouter) -> TestRouterGuard {
    *test_router_slot().write() = Some(router);
    TestRouterGuard
}

impl EmbeddingRouter {
    pub fn load_default() -> Result<Self> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(router) = test_router_slot().read().clone() {
            return Ok(router);
        }
        let path = config_path();
        if !path.exists() {
            return Ok(Self::default());
        }
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading embed config {}", path.display()))?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(text: &str) -> Result<Self> {
        let file: EmbedConfigFile = toml::from_str(text).context("parsing embed config")?;
        Ok(Self { config: file.embed })
    }

    pub fn route(&self, bucket: Bucket, project_id: Option<&str>) -> Result<Route> {
        let provider_id = self.provider_id(bucket, project_id);
        let Some(alias_config) = self.config.providers.get(provider_id) else {
            bail!(
                "unknown embedding provider `{provider_id}` for bucket {}",
                bucket.as_str()
            );
        };
        let (endpoint_kind, document_model, query_model, dimensions, output_dtype) =
            match &alias_config {
                ProviderAliasConfig::VoyageText(config) => {
                    config.validate(provider_id)?;
                    let (document_model, query_model) = config.resolved_models(provider_id)?;
                    (
                        EmbedEndpointKind::Text,
                        document_model,
                        query_model,
                        config.output_dimension,
                        config.output_dtype,
                    )
                }
                ProviderAliasConfig::VoyageContext(config) => {
                    config.validate(provider_id)?;
                    (
                        EmbedEndpointKind::ContextualizedText,
                        config.model.clone(),
                        config.model.clone(),
                        config.output_dimension,
                        config.output_dtype,
                    )
                }
                ProviderAliasConfig::VoyageMultimodal(config) => {
                    config.validate(provider_id)?;
                    (
                        EmbedEndpointKind::Multimodal,
                        config.model.clone(),
                        config.model.clone(),
                        config.output_dimension,
                        config.output_dtype,
                    )
                }
                ProviderAliasConfig::Ollama(config) => (
                    EmbedEndpointKind::Ollama,
                    config.model.clone(),
                    config.model.clone(),
                    ollama::OLLAMA_DIMENSIONS,
                    OutputDType::Float,
                ),
            };
        let compatibility_family =
            derive_compatibility_family(endpoint_kind, &document_model, dimensions, output_dtype);
        let query_family =
            derive_compatibility_family(endpoint_kind, &query_model, dimensions, output_dtype);
        if compatibility_family != query_family {
            bail!(
                "embedding provider `{provider_id}`: document_model `{document_model}` \
                 (family {compatibility_family}) and query_model `{query_model}` (family \
                 {query_family}) are not in the same compatibility family; asymmetric \
                 retrieval requires a shared vector space"
            );
        }
        Ok(Route {
            bucket,
            project_id: project_id.map(str::to_string),
            provider_id: provider_id.to_string(),
            endpoint_kind,
            document_model,
            query_model,
            dimensions,
            output_dtype,
            compatibility_family,
        })
    }

    pub fn route_for(
        &self,
        bucket: Bucket,
        project_id: Option<&str>,
    ) -> Result<Box<dyn EmbeddingProvider>> {
        // route() owns validation (family agreement, dtype support);
        // provider construction reuses its resolved metadata.
        let route = self.route(bucket, project_id)?;
        let provider_id = route.provider_id.as_str();
        match self.config.providers.get(provider_id) {
            Some(ProviderAliasConfig::VoyageText(config)) => Ok(Box::new(
                voyage::VoyageProvider::from_config(provider_id.to_string(), &config)?,
            )),
            Some(ProviderAliasConfig::VoyageContext(config)) => Ok(Box::new(
                voyage_context::VoyageContextProvider::from_config(
                    provider_id.to_string(),
                    &config,
                )?,
            )),
            Some(ProviderAliasConfig::VoyageMultimodal(config)) => Ok(Box::new(
                voyage_multimodal::VoyageMultimodalProvider::from_config(
                    provider_id.to_string(),
                    &config,
                )?,
            )),
            Some(ProviderAliasConfig::Ollama(config)) => Ok(Box::new(
                ollama::OllamaProvider::from_config(provider_id.to_string(), &config)?,
            )),
            None => bail!(
                "unknown embedding provider `{provider_id}` for bucket {}",
                bucket.as_str()
            ),
        }
    }

    fn provider_id(&self, bucket: Bucket, project_id: Option<&str>) -> &str {
        project_id
            .and_then(|id| self.config.routes.per_project.get(id))
            .and_then(|routes| routes.get(bucket))
            .or_else(|| self.config.routes.global(bucket))
            .unwrap_or(VOYAGE_PROVIDER_ID)
    }

    pub fn rate_limit_per_min(&self, provider_id: &str) -> Option<u32> {
        match self.config.providers.get(provider_id) {
            Some(ProviderAliasConfig::VoyageText(config)) => Some(config.rate_limit_per_min),
            Some(ProviderAliasConfig::VoyageContext(config)) => Some(config.rate_limit_per_min),
            Some(ProviderAliasConfig::VoyageMultimodal(config)) => Some(config.rate_limit_per_min),
            Some(ProviderAliasConfig::Ollama(_)) | None => None,
        }
    }

    /// Model-rerank stage config (`[embed.rerank]`), defaulted when absent.
    pub fn rerank_config(&self) -> rerank::RerankConfig {
        self.config.rerank.clone().unwrap_or_default()
    }

    /// Cheap route metadata for one visual chunk kind
    /// (`[embed.routes.visual]`): the visual-routing analog of `route()`:
    /// no HTTP client, no provider construction, just enough to derive a
    /// partition id and report status. `Ok(None)` when the kind is unrouted
    /// (visual embedding is opt-in per kind); an alias that is not
    /// multimodal-typed is a config error: visual payloads must never
    /// silently route into a text space.
    pub fn visual_route(&self, kind: &str) -> Result<Option<VisualRouteMeta>> {
        let Some(alias) = self.config.routes.visual.get(kind) else {
            return Ok(None);
        };
        match self.config.providers.get(alias) {
            Some(ProviderAliasConfig::VoyageMultimodal(config)) => {
                config.validate(alias)?;
                let compatibility_family = derive_compatibility_family(
                    EmbedEndpointKind::Multimodal,
                    &config.model,
                    config.output_dimension,
                    config.output_dtype,
                );
                Ok(Some(VisualRouteMeta {
                    provider_id: alias.clone(),
                    document_model: config.model.clone(),
                    dimensions: config.output_dimension,
                    output_dtype: config.output_dtype,
                    compatibility_family,
                }))
            }
            Some(_) => bail!(
                "visual route `{kind}` maps to provider `{alias}`, which is not a \
                 multimodal provider; visual kinds require a multimodal route family"
            ),
            None => bail!("unknown embedding provider `{alias}` for visual route `{kind}`"),
        }
    }

    /// Provider for one visual chunk kind (`[embed.routes.visual]`). See
    /// `visual_route` for the metadata-only, provider-construction-free
    /// variant; use that one when only the partition id / status fields are
    /// needed (e.g. once per enqueue) and reserve this one for the
    /// once-per-worker provider build (matches the `route()`/`route_for()`
    /// split for the bucket-keyed text routes).
    pub fn visual_provider(&self, kind: &str) -> Result<Option<Box<dyn EmbeddingProvider>>> {
        if self.visual_route(kind)?.is_none() {
            return Ok(None);
        }
        // Re-resolve the alias config: visual_route() already validated it
        // is a VoyageMultimodal alias, so the arms below are exhaustive in
        // practice even though the match stays defensive.
        let alias = self
            .config
            .routes
            .visual
            .get(kind)
            .expect("visual_route confirmed this alias exists");
        match self.config.providers.get(alias) {
            Some(ProviderAliasConfig::VoyageMultimodal(config)) => Ok(Some(Box::new(
                voyage_multimodal::VoyageMultimodalProvider::from_config(alias.clone(), &config)?,
            ))),
            _ => unreachable!("visual_route already validated this alias is VoyageMultimodal"),
        }
    }

    /// Every distinct visual partition the current config maps
    /// (`[embed.routes.visual]`), deduped by `vector_route_id` — multiple
    /// chunk kinds (`image`, `pdf_figure`, ...) commonly share one
    /// multimodal alias, and a partition-keyed consumer (retrieval,
    /// partition lifecycle tooling) must search or account for it once, not
    /// once per kind. Best-effort like `configured_routes()`: a kind whose
    /// alias fails to resolve is skipped. Returns
    /// `(vector_route_id, representative_kind, meta)`; the representative
    /// kind is whichever configured kind resolves to that partition first
    /// (stable `BTreeMap` iteration order) — callers needing a provider use
    /// it with `visual_provider(kind)`.
    pub fn configured_visual_routes(&self) -> Vec<(String, String, VisualRouteMeta)> {
        let mut seen = BTreeMap::<String, (String, VisualRouteMeta)>::new();
        for kind in self.config.routes.visual.keys() {
            if let Ok(Some(meta)) = self.visual_route(kind) {
                seen.entry(meta.vector_route_id())
                    .or_insert_with(|| (kind.clone(), meta));
            }
        }
        seen.into_iter()
            .map(|(route_id, (kind, meta))| (route_id, kind, meta))
            .collect()
    }

    /// Every route the current config maps: all buckets globally, plus
    /// every per-project override. Best-effort — a bucket whose alias
    /// fails to resolve is skipped (it cannot claim a partition either).
    /// This is the "is any bucket mapped to this partition?" source for
    /// partition lifecycle tooling.
    pub fn configured_routes(&self) -> Vec<Route> {
        let mut routes = Vec::new();
        for bucket in Bucket::ALL {
            if let Ok(route) = self.route(bucket, None) {
                routes.push(route);
            }
        }
        let project_ids = self
            .config
            .routes
            .per_project
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        for project_id in project_ids {
            for bucket in Bucket::ALL {
                if let Ok(route) = self.route(bucket, Some(&project_id)) {
                    routes.push(route);
                }
            }
        }
        routes
    }

    pub fn queue_and_vector_route(
        &self,
        bucket: Bucket,
        project_id: Option<&str>,
    ) -> Result<(String, String)> {
        let route = self.route(bucket, project_id)?;
        let default = self.route(bucket, None)?;
        let queue_route = if project_id.is_some()
            && (route.vector_route_id() != default.vector_route_id()
                || route.query_model != default.query_model)
        {
            format!("{}:{}", bucket.as_str(), project_id.unwrap_or_default())
        } else {
            bucket.as_str().to_string()
        };
        Ok((queue_route, route.vector_route_id()))
    }
}

pub fn route_for(bucket: Bucket, project_id: Option<&str>) -> Result<Box<dyn EmbeddingProvider>> {
    EmbeddingRouter::load_default()?.route_for(bucket, project_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterId {
    pub id: String,
    pub members: Vec<EntityRef>,
}

pub fn embed_iterate_internal(
    bucket: &str,
    project_id: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<impl Iterator<Item = (EntityRef, Vec<f32>)> + use<>> {
    embed_iterate_internal_with(&EmbeddingRouter::load_default()?, bucket, project_id, since)
}

/// Router-injected variant so tests never resolve routes through the
/// host's real embed.toml (per-test stores must pair with a fixed router).
pub fn embed_iterate_internal_with(
    router: &EmbeddingRouter,
    bucket: &str,
    project_id: &str,
    since: Option<chrono::DateTime<chrono::Utc>>,
) -> Result<impl Iterator<Item = (EntityRef, Vec<f32>)> + use<>> {
    let route = internal_bucket_route(router, bucket, project_id)?;
    let rows = bbox_vectors::iter_active(&route, since)?
        .filter_map(|entry| {
            vector_entity_ref(&entry.entity_id).map(|entity| (entity, entry.vector))
        })
        .collect::<Vec<_>>();
    Ok(rows.into_iter())
}

pub fn cluster_neighbors_within(
    bucket: &str,
    project_id: &str,
    similarity_threshold: f32,
) -> Result<Vec<ClusterId>> {
    cluster_neighbors_within_router(
        &EmbeddingRouter::load_default()?,
        bucket,
        project_id,
        similarity_threshold,
    )
}

/// Router-injected variant; see `embed_iterate_internal_with`.
pub fn cluster_neighbors_within_router(
    router: &EmbeddingRouter,
    bucket: &str,
    project_id: &str,
    similarity_threshold: f32,
) -> Result<Vec<ClusterId>> {
    let route = internal_bucket_route(router, bucket, project_id)?;
    let clusters = bbox_vectors::cluster_neighbors_within_route(&route, similarity_threshold)?
        .into_iter()
        .filter_map(|cluster| {
            let members = cluster
                .members
                .iter()
                .filter_map(|raw| vector_entity_ref(raw))
                .collect::<Vec<_>>();
            if members.len() < 2 {
                None
            } else {
                Some(ClusterId {
                    id: cluster.id,
                    members,
                })
            }
        })
        .collect();
    Ok(clusters)
}

fn internal_bucket_route(
    router: &EmbeddingRouter,
    bucket: &str,
    project_id: &str,
) -> Result<String> {
    let bucket = bucket_from_str(bucket)?;
    let project = if project_id.trim().is_empty() {
        None
    } else {
        Some(project_id.trim())
    };
    Ok(router.route(bucket, project)?.vector_route_id())
}

fn bucket_from_str(bucket: &str) -> Result<Bucket> {
    let bucket = bucket.trim();
    Bucket::ALL
        .iter()
        .copied()
        .find(|candidate| candidate.as_str() == bucket)
        .with_context(|| {
            format!(
                "unknown embedding bucket `{bucket}`; expected one of: {}",
                Bucket::ALL
                    .iter()
                    .map(|candidate| candidate.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
}

fn vector_entity_ref(raw: &str) -> Option<EntityRef> {
    if let Some((name, version, _component)) =
        crate::embed_queue::parse_agent_component_entity_id_parts(raw)
    {
        return Some(EntityRef::Agent { name, version });
    }
    EntityRef::parse(raw).ok()
}

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".config")
        })
        .join("blackbox")
        .join("embed.toml")
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RouteDimensionTracker {
    dimensions: BTreeMap<String, usize>,
}

impl RouteDimensionTracker {
    pub fn record(&mut self, route: impl Into<String>, dimensions: usize) -> Result<()> {
        let route = route.into();
        if let Some(existing) = self.dimensions.get(&route) {
            if *existing != dimensions {
                bail!(
                    "embedding route `{route}` dimension mismatch: existing={existing}, incoming={dimensions}; run bbox_reembed for the route before mixing vectors"
                );
            }
        } else {
            self.dimensions.insert(route, dimensions);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_route_is_voyage() {
        let router = EmbeddingRouter::default();
        let route = router.route(Bucket::Knowledge, None).unwrap();
        assert_eq!(route.provider_id, VOYAGE_PROVIDER_ID);
        assert_eq!(route.dimensions, voyage::VOYAGE_DIMENSIONS);
        assert_eq!(route.document_model, "voyage-code-3");
        assert_eq!(route.query_model, "voyage-code-3");
        assert_eq!(route.endpoint_kind, EmbedEndpointKind::Text);
        assert_eq!(route.output_dtype, OutputDType::Float);
        assert_eq!(route.compatibility_family, "voyage-code-3:1024:float");
    }

    /// The default route's partition id must not move: this is the live
    /// partition every pre-existing vector sits in. Golden, not derived —
    /// if this changes, deployed corpora orphan silently.
    #[test]
    fn default_vector_route_id_is_stable_across_metadata_growth() {
        let route = EmbeddingRouter::default()
            .route(Bucket::Knowledge, None)
            .unwrap();
        assert_eq!(
            route.vector_route_id(),
            "voyage-voyage-code-3-1024-0103683e"
        );
    }

    #[test]
    fn non_float_dtype_forces_a_new_partition_and_family() {
        let float = EmbeddingRouter::default()
            .route(Bucket::Knowledge, None)
            .unwrap();
        let quantized = Route {
            output_dtype: OutputDType::Int8,
            compatibility_family: derive_compatibility_family(
                float.endpoint_kind,
                &float.document_model,
                float.dimensions,
                OutputDType::Int8,
            ),
            ..float.clone()
        };
        assert_ne!(float.vector_route_id(), quantized.vector_route_id());
        assert_ne!(float.compatibility_family, quantized.compatibility_family);
        assert!(quantized.vector_route_id().contains("int8"));
    }

    #[test]
    fn legacy_voyage_and_ollama_tables_still_parse() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.providers.voyage]
model = "voyage-code-2"
rate_limit_per_min = 500

[embed.providers.ollama]
model = "custom-local"

[embed.routes]
notes = "ollama"
"#,
        )
        .unwrap();
        let route = router.route(Bucket::Knowledge, None).unwrap();
        assert_eq!(route.provider_id, VOYAGE_PROVIDER_ID);
        assert_eq!(route.document_model, "voyage-code-2");
        assert_eq!(router.rate_limit_per_min(VOYAGE_PROVIDER_ID), Some(500));
        let notes = router.route(Bucket::Notes, None).unwrap();
        assert_eq!(notes.document_model, "custom-local");
        assert_eq!(notes.endpoint_kind, EmbedEndpointKind::Ollama);
        assert_eq!(notes.compatibility_family, "ollama:custom-local:768:float");
    }

    /// Two Voyage-backed aliases with different models — the core Layer 0
    /// capability the old `ProviderConfigs { voyage, ollama }` shape could
    /// not express.
    #[test]
    fn two_voyage_aliases_route_to_different_models_and_partitions() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.providers.voyage_code]
type = "voyage_text"
model = "voyage-code-3"

[embed.providers.voyage_text]
type = "voyage_text"
document_model = "voyage-4-large"
query_model = "voyage-4-lite"

[embed.routes]
code = "voyage_code"
knowledge = "voyage_text"
"#,
        )
        .unwrap();
        let code = router.route(Bucket::Code, None).unwrap();
        let knowledge = router.route(Bucket::Knowledge, None).unwrap();
        assert_eq!(code.document_model, "voyage-code-3");
        assert_eq!(knowledge.document_model, "voyage-4-large");
        assert_eq!(knowledge.query_model, "voyage-4-lite");
        assert_eq!(knowledge.compatibility_family, "voyage-4:1024:float");
        assert_ne!(code.vector_route_id(), knowledge.vector_route_id());
        assert_ne!(code.compatibility_family, knowledge.compatibility_family);
    }

    #[test]
    fn asymmetric_pair_across_families_is_rejected_at_load() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.providers.bad_pair]
type = "voyage_text"
document_model = "voyage-4-large"
query_model = "voyage-code-3"

[embed.routes]
knowledge = "bad_pair"
"#,
        )
        .unwrap();
        let err = router.route(Bucket::Knowledge, None).unwrap_err();
        assert!(
            err.to_string().contains("compatibility family"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn unknown_route_alias_is_an_error_not_a_fallback() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
knowledge = "no_such_provider"
"#,
        )
        .unwrap();
        let err = router.route(Bucket::Knowledge, None).unwrap_err();
        assert!(err.to_string().contains("no_such_provider"));
    }

    #[test]
    fn new_alias_without_type_is_rejected() {
        let err = EmbeddingRouter::from_toml_str(
            r#"
[embed.providers.mystery]
model = "who-knows"
"#,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("missing required field `type`")
                || err
                    .root_cause()
                    .to_string()
                    .contains("missing required field `type`"),
            "unexpected error: {err:#}"
        );
    }

    #[test]
    fn builtin_aliases_resolve_without_config() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
knowledge = "voyage_text"
code = "voyage_code"
"#,
        )
        .unwrap();
        let knowledge = router.route(Bucket::Knowledge, None).unwrap();
        assert_eq!(knowledge.document_model, "voyage-4-large");
        assert_eq!(knowledge.query_model, "voyage-4-lite");
        assert_eq!(knowledge.compatibility_family, "voyage-4:1024:float");
        let code = router.route(Bucket::Code, None).unwrap();
        assert_eq!(code.document_model, "voyage-code-3");
    }

    #[test]
    fn configured_routes_cover_global_and_per_project_mappings() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
code = "voyage_code"

[embed.routes.per_project."proj1234"]
threads = "ollama"
"#,
        )
        .unwrap();
        let routes = router.configured_routes();
        // 8 global buckets + 8 per-project rows for proj1234.
        assert_eq!(routes.len(), 16);
        assert!(routes.iter().any(|route| {
            route.bucket == Bucket::Threads
                && route.project_id.as_deref() == Some("proj1234")
                && route.provider_id == OLLAMA_PROVIDER_ID
        }));
        assert!(
            routes
                .iter()
                .any(|route| route.bucket == Bucket::Code && route.project_id.is_none())
        );
    }

    #[test]
    fn contextualized_alias_routes_with_own_family() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
code = "voyage_context"
"#,
        )
        .unwrap();
        let route = router.route(Bucket::Code, None).unwrap();
        assert_eq!(route.endpoint_kind, EmbedEndpointKind::ContextualizedText);
        assert_eq!(route.document_model, "voyage-context-4");
        assert_eq!(route.query_model, "voyage-context-4");
        assert_eq!(route.compatibility_family, "voyage-context-4:1024:float");
        // Its own partition, never shared with voyage-4 text or code-3.
        assert!(route.vector_route_id().starts_with("voyage_context-"));
    }

    #[test]
    fn visual_route_metadata_matches_the_constructed_provider() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let meta = router.visual_route("pdf_figure").unwrap().unwrap();
        let provider = router.visual_provider("pdf_figure").unwrap().unwrap();
        assert_eq!(meta.provider_id, provider.id());
        assert_eq!(meta.document_model, provider.document_model());
        assert_eq!(meta.dimensions, provider.dimensions());
        assert_eq!(meta.compatibility_family, provider.compatibility_family());
        assert_eq!(
            meta.vector_route_id(),
            vector_route_id_for(
                provider.id(),
                provider.document_model(),
                provider.dimensions(),
                provider.output_dtype(),
            )
        );
        assert!(router.visual_route("video_segment").unwrap().is_none());
    }

    #[test]
    fn visual_routes_resolve_multimodal_aliases_only() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
pdf_figure = "voyage_visual"
slide_image = "voyage_text"
"#,
        )
        .unwrap();
        let provider = router.visual_provider("pdf_figure").unwrap().unwrap();
        assert_eq!(provider.endpoint_kind(), EmbedEndpointKind::Multimodal);
        assert_eq!(provider.document_model(), "voyage-multimodal-3.5");
        assert_eq!(
            provider.compatibility_family(),
            "voyage-multimodal-3.5:1024:float"
        );
        // Unrouted kinds are opt-out, not errors.
        assert!(router.visual_provider("video_segment").unwrap().is_none());
        // A text alias behind a visual kind is a hard config error.
        let err = match router.visual_provider("slide_image") {
            Err(err) => err,
            Ok(_) => panic!("text alias behind a visual kind must be rejected"),
        };
        assert!(err.to_string().contains("not a multimodal provider"));
    }

    /// `image` and `pdf_figure` sharing one multimodal alias must dedupe to
    /// ONE partition entry — a partition-keyed retrieval lane (hybrid
    /// search) must search it once, not once per configured kind.
    #[test]
    fn configured_visual_routes_dedupes_kinds_sharing_one_partition() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
image = "voyage_visual"
pdf_figure = "voyage_visual"
"#,
        )
        .unwrap();
        let routes = router.configured_visual_routes();
        assert_eq!(routes.len(), 1, "one partition, two kinds mapping to it");
        let (route_id, kind, meta) = &routes[0];
        assert_eq!(meta.provider_id, "voyage_visual");
        assert_eq!(meta.document_model, "voyage-multimodal-3.5");
        assert_eq!(*route_id, meta.vector_route_id());
        // Representative kind is whichever sorts first (BTreeMap order).
        assert_eq!(kind, "image");
    }

    /// A visual kind that fails to resolve (unknown alias) is skipped, not
    /// propagated as an error — best-effort like `configured_routes()`.
    #[test]
    fn configured_visual_routes_skips_unresolvable_kinds() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes.visual]
pdf_figure = "no_such_alias"
"#,
        )
        .unwrap();
        assert!(router.configured_visual_routes().is_empty());
    }

    #[test]
    fn compatibility_family_derivation_matrix() {
        let cases = [
            ("voyage-4", "voyage-4:1024:float"),
            ("voyage-4-large", "voyage-4:1024:float"),
            ("voyage-4-lite", "voyage-4:1024:float"),
            ("voyage-4-nano", "voyage-4:1024:float"),
            ("voyage-code-3", "voyage-code-3:1024:float"),
            ("voyage-context-4", "voyage-context-4:1024:float"),
            ("voyage-multimodal-3.5", "voyage-multimodal-3.5:1024:float"),
            ("voyage-40", "voyage-40:1024:float"), // prefix must not overmatch
        ];
        for (model, expected) in cases {
            assert_eq!(
                derive_compatibility_family(
                    EmbedEndpointKind::Text,
                    model,
                    1024,
                    OutputDType::Float
                ),
                expected,
                "model {model}"
            );
        }
        assert_eq!(
            derive_compatibility_family(
                EmbedEndpointKind::Text,
                "voyage-4-large",
                512,
                OutputDType::Binary
            ),
            "voyage-4:512:binary"
        );
    }

    #[test]
    fn per_project_override_beats_global_route() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
[embed.routes]
threads = "voyage"

[embed.routes.per_project."proj1234"]
threads = "ollama"
"#,
        )
        .unwrap();
        assert_eq!(
            router
                .route(Bucket::Threads, Some("proj1234"))
                .unwrap()
                .provider_id,
            OLLAMA_PROVIDER_ID
        );
        assert_eq!(
            router.route(Bucket::Threads, None).unwrap().provider_id,
            VOYAGE_PROVIDER_ID
        );
    }

    #[test]
    fn dimension_tracker_rejects_mismatch() {
        let mut tracker = RouteDimensionTracker::default();
        tracker.record("knowledge", 1024).unwrap();
        let err = tracker.record("knowledge", 768).unwrap_err();
        assert!(err.to_string().contains("dimension mismatch"));
    }

    #[test]
    fn embed_iterate_internal_respects_since_and_entity_refs() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = bbox_vectors::install_test_global(store.clone());
        let route = EmbeddingRouter::default()
            .route(Bucket::Transcripts, None)
            .unwrap()
            .vector_route_id();
        store
            .upsert(
                &route,
                "transcript:claude:old-session:1:0",
                "old",
                vec![1.0, 0.0],
            )
            .unwrap();
        let cutoff = chrono::Utc::now();
        std::thread::sleep(std::time::Duration::from_millis(2));
        store
            .upsert(
                &route,
                "transcript:claude:new-session:2:0",
                "new",
                vec![0.0, 1.0],
            )
            .unwrap();

        let rows = embed_iterate_internal_with(
            &EmbeddingRouter::default(),
            "transcripts",
            "",
            Some(cutoff),
        )
        .unwrap()
        .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].0,
            EntityRef::Transcript {
                provider: "claude".into(),
                session_id: "new-session".into(),
                line_offset: 2,
                event_idx: 0,
            }
        );
    }

    #[test]
    fn cluster_neighbors_within_returns_bounded_entity_clusters() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_vectors::VectorStore::open(dir.path()).unwrap());
        let _guard = bbox_vectors::install_test_global(store.clone());
        let route = EmbeddingRouter::default()
            .route(Bucket::AgentManifest, None)
            .unwrap()
            .vector_route_id();
        store
            .upsert(
                &route,
                "agent_embed:reviewer:v1:primary",
                "a",
                vec![1.0, 0.0],
            )
            .unwrap();
        store
            .upsert(
                &route,
                "agent_embed:copywriter:v1:primary",
                "b",
                vec![1.0, 0.0],
            )
            .unwrap();
        store
            .upsert(&route, "agent_embed:writer:v1:primary", "c", vec![0.0, 1.0])
            .unwrap();

        let clusters = cluster_neighbors_within_router(
            &EmbeddingRouter::default(),
            "agent_manifest",
            "",
            0.99,
        )
        .unwrap();
        assert_eq!(clusters.len(), 1);
        assert_eq!(
            clusters[0].members,
            vec![
                EntityRef::Agent {
                    name: "copywriter".into(),
                    version: 1,
                },
                EntityRef::Agent {
                    name: "reviewer".into(),
                    version: 1,
                }
            ]
        );
    }
}

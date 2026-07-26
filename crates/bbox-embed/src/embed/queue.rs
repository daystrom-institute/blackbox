use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use async_trait::async_trait;
use bbox_visual_store::VisualPayloadRef;
use parking_lot::RwLock;
use rmcp::schemars;
use serde::Serialize;
use tokio::sync::mpsc;

use super::{
    Bucket, EmbedEndpointKind, EmbedInput, EmbedInputType, EmbedOutput, EmbeddingProvider,
    EmbeddingRouter, MultimodalPart,
};

const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(5);
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BATCH_RETRIES: u8 = 3;
const MAX_ROUTE_QUEUE_DEPTH: u64 = 10_000;
const MAX_ROUTE_QUEUE_BYTES: u64 = 128 * 1024 * 1024;

/// Text the embedding providers will accept. Voyage rejects empty strings
/// with HTTP 400 ("Input cannot contain empty strings"), and the rejection
/// drops the whole batch — one empty chunk stranded 51 batch-mates
/// (gap-e3e033ce). Whitespace-only text embeds to noise anyway. Coverage
/// computation uses the same predicate so skipped items don't read as
/// missing residue forever.
pub fn embeddable_text(text: &str) -> bool {
    !text.trim().is_empty()
}

/// Marker for provider failures no retry can fix — payload-level HTTP 4xx
/// rejections (not 408/429). Providers attach this to the error chain so
/// the worker bisects the batch to the poison item(s) instead of retrying
/// a request that will fail identically three times and then dropping
/// every batch-mate with it (gap-e3e033ce).
#[derive(Debug)]
pub struct NonRetryableBatchError;

impl std::fmt::Display for NonRetryableBatchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("non-retryable embedding batch rejection")
    }
}

impl std::error::Error for NonRetryableBatchError {}

fn is_non_retryable(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|cause| cause.downcast_ref::<NonRetryableBatchError>().is_some())
}

/// Seed a route's status with the provider's routing identity so
/// `bbox_embed_status` reports what actually embeds and searches the route.
fn provider_route_status(provider: &dyn EmbeddingProvider) -> RouteStatus {
    let query_model = (provider.query_model() != provider.document_model())
        .then(|| provider.query_model().to_string());
    RouteStatus {
        provider: Some(provider.id().to_string()),
        model: Some(provider.document_model().to_string()),
        query_model,
        endpoint_kind: Some(provider.endpoint_kind()),
        output_dtype: Some(provider.output_dtype().as_str().to_string()),
        compatibility_family: Some(provider.compatibility_family()),
        dim: Some(provider.dimensions()),
        ..RouteStatus::default()
    }
}

type ProviderSpec = (String, Arc<dyn EmbeddingProvider>, Option<u32>, String);

/// Queue workers embed stored corpus content: every batch is `Text` inputs
/// with the `Document` retrieval role. One vector per text.
async fn embed_document_texts(
    provider: &dyn EmbeddingProvider,
    texts: &[String],
) -> Result<Vec<Vec<f32>>> {
    let inputs = texts
        .iter()
        .map(|text| EmbedInput::Text(text.clone()))
        .collect::<Vec<_>>();
    provider
        .embed_batch(&inputs, EmbedInputType::Document)
        .await?
        .into_iter()
        .map(EmbedOutput::into_single)
        .collect()
}

/// Queue route label for a visual chunk kind. A dedicated function (rather
/// than inlining `format!`) so the label is spelled identically everywhere
/// it's constructed (`resolve_visual_route`, status reporting).
fn visual_queue_route(kind: &str) -> String {
    format!("visual:{kind}")
}

/// Document identity for contextualized grouping: the chunk entity id
/// minus its trailing numeric chunk index (`project_file:p:f:h:3` and
/// `:4` share a document; ids without a numeric tail are their own
/// document). Derived here so enqueue sites don't have to thread a
/// grouping key through every corpus enumerator.
fn document_group_key(entity_id: &str) -> &str {
    match entity_id.rsplit_once(':') {
        Some((prefix, suffix))
            if !prefix.is_empty()
                && !suffix.is_empty()
                && suffix.chars().all(|ch| ch.is_ascii_digit()) =>
        {
            prefix
        }
        _ => entity_id,
    }
}

/// Embed one packed batch with the role and shape its provider expects.
/// Text/Ollama providers take flat `Text` inputs; contextualized providers
/// take consecutive same-document runs as `DocumentChunks` groups so each
/// chunk is encoded with its document context; multimodal providers take
/// interleaved `Multimodal` parts built from the request's
/// `VisualPayloadRef` (bytes loaded from the visual payload sidecar at this
/// point, not held in the pending queue). Returns one vector per request,
/// in batch order.
async fn embed_batch_requests(
    provider: &dyn EmbeddingProvider,
    batch: &[EmbedRequest],
) -> Result<Vec<Vec<f32>>> {
    if provider.endpoint_kind() == EmbedEndpointKind::Multimodal {
        return embed_visual_requests(provider, batch).await;
    }
    if provider.endpoint_kind() != EmbedEndpointKind::ContextualizedText {
        let texts = batch.iter().map(|req| req.text.clone()).collect::<Vec<_>>();
        return embed_document_texts(provider, &texts).await;
    }
    let mut inputs: Vec<EmbedInput> = Vec::new();
    let mut group_sizes: Vec<usize> = Vec::new();
    let mut current_key: Option<&str> = None;
    let mut current: Vec<String> = Vec::new();
    for request in batch {
        let key = document_group_key(&request.entity_id);
        if current_key != Some(key) && !current.is_empty() {
            group_sizes.push(current.len());
            inputs.push(EmbedInput::DocumentChunks(std::mem::take(&mut current)));
        }
        current_key = Some(key);
        current.push(request.text.clone());
    }
    if !current.is_empty() {
        group_sizes.push(current.len());
        inputs.push(EmbedInput::DocumentChunks(current));
    }
    let outputs = provider
        .embed_batch(&inputs, EmbedInputType::Document)
        .await?;
    if outputs.len() != group_sizes.len() {
        anyhow::bail!(
            "contextualized provider returned {} document outputs, expected {}",
            outputs.len(),
            group_sizes.len()
        );
    }
    let mut vectors = Vec::with_capacity(batch.len());
    for (output, expected) in outputs.into_iter().zip(group_sizes) {
        if output.vectors.len() != expected {
            anyhow::bail!(
                "contextualized provider returned {} vectors for a {expected}-chunk document",
                output.vectors.len()
            );
        }
        vectors.extend(output.vectors);
    }
    Ok(vectors)
}

/// Multimodal batches: load each request's payload bytes from the visual
/// sidecar store at request-build time (never held earlier: `EmbedRequest`
/// carries only the small `VisualPayloadRef`, per the design's "chunks and
/// embed requests carry a ref instead of inline bytes"), normalize them
/// against the provider's constraints (gap-48ae5495), build interleaved
/// `Multimodal` parts, and embed. A request with no `visual_payload` is a
/// caller bug (only the visual enqueue path should route here), not a
/// transient failure: bail rather than silently skipping it.
async fn embed_visual_requests(
    provider: &dyn EmbeddingProvider,
    batch: &[EmbedRequest],
) -> Result<Vec<Vec<f32>>> {
    let store = bbox_visual_store::global();
    let mut inputs = Vec::with_capacity(batch.len());
    for request in batch {
        let Some(payload) = &request.visual_payload else {
            bail!(
                "visual embed route received a request with no visual payload \
                 (entity_id={})",
                request.entity_id
            );
        };
        let path = store.path_for(&payload.content_hash);
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("reading visual payload {}", path.display()))?;
        let (bytes, mime) =
            normalize_visual_bytes(bytes, payload.media_type.clone(), &request.entity_id).await?;
        let mut parts = Vec::with_capacity(2);
        if !request.text.trim().is_empty() {
            parts.push(MultimodalPart::Text(request.text.clone()));
        }
        parts.push(MultimodalPart::ImageBytes { mime, bytes });
        inputs.push(EmbedInput::Multimodal(parts));
    }
    provider
        .embed_batch(&inputs, EmbedInputType::Document)
        .await?
        .into_iter()
        .map(EmbedOutput::into_single)
        .collect()
}

/// Normalize one visual payload against the provider's pixel-cap and
/// aspect-ratio constraints (gap-48ae5495) before it ever reaches
/// `voyage_multimodal::preflight_part`: downscales images over the pixel
/// cap and pads thin scan-strip images into the permitted aspect-ratio
/// range, instead of letting them be poison-dropped. Image decode/
/// resize/encode is CPU-bound, so it runs on the blocking pool rather than
/// inline on this async worker (concurrency-model I2). A normalization
/// failure (undecodable bytes, e.g. a video mislabeled as an image) falls
/// back to the original bytes/mime unchanged; `preflight_part` still
/// poison-rejects whatever still violates a constraint.
async fn normalize_visual_bytes(
    bytes: Vec<u8>,
    mime: String,
    entity_id: &str,
) -> Result<(Vec<u8>, String)> {
    let outcome = tokio::task::spawn_blocking(move || {
        super::visual_normalize::normalize_image_bytes(bytes, mime)
    })
    .await
    .with_context(|| {
        format!("visual payload normalization task panicked (entity_id={entity_id})")
    })?;
    match outcome {
        super::visual_normalize::NormalizeOutcome::Unchanged { bytes, mime }
        | super::visual_normalize::NormalizeOutcome::Normalized { bytes, mime } => {
            Ok((bytes, mime))
        }
        super::visual_normalize::NormalizeOutcome::Failed {
            bytes,
            mime,
            reason,
        } => {
            tracing::debug!(
                entity_id,
                reason,
                "visual payload normalization skipped; preflight will judge the original bytes"
            );
            Ok((bytes, mime))
        }
    }
}

/// Which packing shape a route's provider needs. `Grouped` (contextualized
/// routes) never splits one document's consecutive chunk run across
/// batches; `Flat` and `Visual` are both simple count/byte-capped runs,
/// differing only in caps and how request weight is measured (`Visual`
/// weighs by payload byte length, not caption text length).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PackMode {
    Flat,
    Grouped,
    Visual,
}

/// Byte weight of one request for batch-cap accounting. Visual requests
/// carry the real payload size on `visual_payload`; their `text` is only a
/// filename-stem caption and would grossly understate the request's actual
/// network weight.
fn request_weight_bytes(request: &EmbedRequest) -> usize {
    request
        .visual_payload
        .as_ref()
        .map(|payload| payload.byte_len as usize)
        .unwrap_or(request.text.len())
}

/// Pop one batch off the pending queue under the count/byte caps.
/// `PackMode::Grouped` (contextualized routes) additionally never splits
/// one document's consecutive chunk run across batches: a run that would
/// straddle the cap closes the batch instead, and a run that exceeds the
/// caps on its own falls back to windowed sub-documents (each window is
/// embedded as its own document, trading context width for progress).
fn pack_batch(pending: &mut VecDeque<EmbedRequest>, mode: PackMode) -> Vec<EmbedRequest> {
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    if mode != PackMode::Grouped {
        let (max_docs, max_bytes) = match mode {
            PackMode::Visual => (MAX_VISUAL_BATCH_DOCS, MAX_VISUAL_BATCH_BYTES),
            _ => (MAX_BATCH_DOCS, MAX_BATCH_BYTES),
        };
        while let Some(req) = pending.pop_front() {
            let req_bytes = request_weight_bytes(&req);
            if !batch.is_empty() && (batch.len() >= max_docs || bytes + req_bytes > max_bytes) {
                pending.push_front(req);
                break;
            }
            bytes += req_bytes;
            batch.push(req);
            if batch.len() >= max_docs {
                break;
            }
        }
        return batch;
    }
    while let Some(front) = pending.front() {
        let key = document_group_key(&front.entity_id).to_string();
        let mut group = Vec::new();
        let mut group_bytes = 0usize;
        let mut run_truncated = false;
        while pending
            .front()
            .is_some_and(|req| document_group_key(&req.entity_id) == key)
        {
            // One contextualized DOCUMENT must fit the contextual model's
            // own context window (voyage-context-4: 32K tokens, rejected
            // rather than truncated -- verified live 2026-07-11). Cap the
            // run at MAX_DOCUMENT_BYTES; the remainder stays queued as the
            // next window, and the batch closes below so consecutive-run
            // grouping cannot re-merge the halves into one oversized
            // document.
            let next_bytes = pending.front().map(|req| req.text.len()).unwrap_or(0);
            if !group.is_empty() && group_bytes + next_bytes > MAX_DOCUMENT_BYTES {
                run_truncated = true;
                break;
            }
            let req = pending.pop_front().expect("front checked");
            group_bytes += req.text.len();
            group.push(req);
        }
        let fits =
            batch.len() + group.len() <= MAX_BATCH_DOCS && bytes + group_bytes <= MAX_BATCH_BYTES;
        if fits {
            bytes += group_bytes;
            batch.extend(group);
            if run_truncated || batch.len() >= MAX_BATCH_DOCS {
                break;
            }
            continue;
        }
        if run_truncated {
            // The window did not fit this batch either; requeue it whole
            // and close so it leads the next batch.
            for req in group.into_iter().rev() {
                pending.push_front(req);
            }
            break;
        }
        if !batch.is_empty() {
            // Close the batch at the document boundary; the whole run
            // goes back to the queue head in order.
            for req in group.into_iter().rev() {
                pending.push_front(req);
            }
            break;
        }
        // The run alone exceeds the caps: windowed fallback. Take a
        // cap-sized window as this batch's single document, requeue the
        // rest of the run (it becomes the next window).
        let mut rest = Vec::new();
        for req in group {
            let req_bytes = req.text.len();
            if !rest.is_empty()
                || batch.len() >= MAX_BATCH_DOCS
                || (!batch.is_empty() && bytes + req_bytes > MAX_BATCH_BYTES)
            {
                rest.push(req);
                continue;
            }
            bytes += req_bytes;
            batch.push(req);
        }
        for req in rest.into_iter().rev() {
            pending.push_front(req);
        }
        break;
    }
    batch
}

/// Outcome of a single enqueue attempt. Distinguishes the three cases the
/// old `bool` return conflated so the residue sweeper can tell convergence
/// from backpressure instead of treating a full-queue rejection as a silent
/// drop (gap-7323e96c): `Skipped` (dedup/empty — no work needed, counts as
/// converged), `QueueFull` (capped — residue remains, come back after drain),
/// and `Error` (route unresolvable / worker stopped / not configured).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EnqueueOutcome {
    Enqueued,
    Skipped,
    QueueFull,
    Error,
}

impl EnqueueOutcome {
    /// True only when the request was actually placed on a route queue. The
    /// `bool`-returning `enqueue` wrapper preserves the historic API by
    /// collapsing every non-`Enqueued` outcome to `false`.
    pub fn enqueued(self) -> bool {
        matches!(self, EnqueueOutcome::Enqueued)
    }
}

#[derive(Debug, Clone)]
pub struct EmbedRequest {
    /// Ignored when `visual_kind` is `Some`: visual routing resolves
    /// through `EmbeddingRouter::visual_route`/`visual_provider` (keyed by
    /// chunk kind, not `Bucket`), not the bucket-keyed text route machinery.
    pub bucket: Bucket,
    pub project_id: Option<String>,
    pub entity_id: String,
    pub chunk_hash: String,
    /// For text requests: the content to embed. For visual requests
    /// (`visual_payload` is `Some`): optional caption text (e.g. the file
    /// name stem) sent alongside the image as a `MultimodalPart::Text`
    /// part; may be empty.
    pub text: String,
    /// `Some(chunk_kind)` routes this request through the visual embedding
    /// lane (`[embed.routes.visual]`) instead of the bucket-keyed text
    /// route. Requires `visual_payload` to also be `Some`.
    pub visual_kind: Option<String>,
    /// Reference to the raw image/video bytes in the content-hash-addressed
    /// visual payload sidecar (`bbox-visual-store`); the queue worker loads
    /// the bytes from disk at request-build time, never earlier.
    pub visual_payload: Option<VisualPayloadRef>,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RouteStatus {
    pub available: bool,
    pub health: String,
    pub health_reason: Option<String>,
    pub provider: Option<String>,
    pub model: Option<String>,
    /// Model used for live queries when the route is asymmetric; absent
    /// when it equals `model`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint_kind: Option<EmbedEndpointKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dtype: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compatibility_family: Option<String>,
    pub dim: Option<usize>,
    pub source_count: Option<u64>,
    pub indexed_count: u64,
    pub session_indexed_count: Option<u64>,
    pub queue_depth: u64,
    pub queue_bytes: u64,
    pub retried_count: u64,
    pub last_error: Option<String>,
    pub coverage_ratio: Option<f32>,
    /// Why `coverage_ratio` is absent when it is deliberately not computed
    /// (e.g. transcripts: the corpus scan is too heavy for a status call).
    /// Distinguishes "guarded" from "broken" (gap-b9d39c10).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage_state: Option<String>,
    /// Cumulative count of items the queue permanently dropped — poison
    /// payloads the provider rejects (HTTP 4xx) and retry-exhausted
    /// batches. Unlike `last_error`, this is NOT cleared by a later
    /// success, so it stays an honest "this much will never embed" signal
    /// (gap-e3e033ce). A nonzero count explains a coverage ratio that
    /// parks below 1.0 with an idle queue — that residue is unembeddable,
    /// not un-enqueued.
    pub dropped_count: u64,
    /// Diagnostic for the most recent permanent drop (entity_id + cause).
    /// Durable like `dropped_count` — survives later successes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_dropped: Option<String>,
    /// Cumulative count of enqueue attempts rejected because the route queue
    /// was at its depth/byte cap. A capped rejection is NOT a drop — the item
    /// simply was not admitted this wave — so it is counted separately from
    /// `dropped_count`. The residue sweeper snapshots this counter around an
    /// enqueue pass to learn "capped, residue remains" without a silent drop
    /// (gap-7323e96c). Not cleared by later successes.
    pub capped_count: u64,
}

impl Default for RouteStatus {
    fn default() -> Self {
        Self {
            available: true,
            health: "ok".into(),
            health_reason: None,
            provider: None,
            model: None,
            query_model: None,
            endpoint_kind: None,
            output_dtype: None,
            compatibility_family: None,
            dim: None,
            source_count: None,
            indexed_count: 0,
            session_indexed_count: None,
            queue_depth: 0,
            queue_bytes: 0,
            retried_count: 0,
            last_error: None,
            coverage_ratio: None,
            coverage_state: None,
            dropped_count: 0,
            last_dropped: None,
            capped_count: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EmbedStatusResponse {
    pub routes: BTreeMap<String, RouteStatus>,
}

/// Post-upsert contradiction hook: the daemon registers its
/// knowledge-contradiction detector here at boot (dependency inversion —
/// detection needs daemon state this layer must not name). Unregistered
/// means no detection, matching a daemon that never installed state.
static CONTRADICTION_HOOK: std::sync::OnceLock<fn(&EmbedRequest, &str, &[f32])> =
    std::sync::OnceLock::new();

/// Register the contradiction detector. Idempotent; first registration wins.
pub fn register_contradiction_hook(hook: fn(&EmbedRequest, &str, &[f32])) {
    let _ = CONTRADICTION_HOOK.set(hook);
}

/// Fired when a route's pending depth returns to zero, i.e. a wave finished
/// draining. The daemon registers a hook here at boot that wakes the residue
/// sweeper so an across-wave refill happens promptly instead of waiting for
/// the next timer tick (gap-7323e96c). Dependency inversion — the queue must
/// not name the daemon's `Notify`. Unregistered means no wake, matching a
/// daemon that never installed the sweeper.
static QUEUE_DRAIN_HOOK: std::sync::OnceLock<fn(&str)> = std::sync::OnceLock::new();

/// Register the queue-drain wake hook. Idempotent; first registration wins.
pub fn register_queue_drain_hook(hook: fn(&str)) {
    let _ = QUEUE_DRAIN_HOOK.set(hook);
}

#[derive(Clone)]
pub struct EmbedQueueHandle {
    inner: Arc<EmbedQueueInner>,
}

struct EmbedQueueInner {
    senders: RwLock<BTreeMap<String, mpsc::UnboundedSender<WorkerCommand>>>,
    statuses: Arc<RwLock<BTreeMap<String, RouteStatus>>>,
    router: Option<EmbeddingRouter>,
    vector_store: Option<Arc<bbox_vectors::VectorStore>>,
    debounce: Duration,
    retry_backoff: Duration,
    /// Per-route depth cap enforced by `try_reserve_queue`. A field (not the
    /// bare `MAX_ROUTE_QUEUE_DEPTH` const) so convergence tests can run the
    /// full cap→drain→refill cycle against a tiny cap without enqueuing 10k
    /// items. Atomic so a test can lower it through the shared `Arc` handle.
    max_queue_depth: std::sync::atomic::AtomicU64,
}

struct ResolvedRoute {
    queue_route: String,
    vector_route: String,
}

enum WorkerCommand {
    Enqueue(EmbedRequest),
    Shutdown,
}

struct WorkerSpec {
    route: String,
    vector_route: String,
    provider: Arc<dyn EmbeddingProvider>,
    rate_limit_per_min: Option<u32>,
    debounce: Duration,
    retry_backoff: Duration,
    statuses: Arc<RwLock<BTreeMap<String, RouteStatus>>>,
    vector_store: Option<Arc<bbox_vectors::VectorStore>>,
    persist_vectors: bool,
}

impl EmbedQueueHandle {
    pub fn start_default() -> Self {
        Self::start_default_with_store(bbox_vectors::global())
    }

    pub fn start_default_with_store(vector_store: Arc<bbox_vectors::VectorStore>) -> Self {
        Self::start_default_with_optional_store(Some(vector_store))
    }

    pub fn start_default_without_store() -> Self {
        Self::start_default_with_optional_store(None)
    }

    /// Build an isolated single-route queue for cross-crate tests.
    ///
    /// The production process always uses the configured router constructors
    /// above. This constructor deliberately requires both an explicit provider
    /// and an explicit vector store so regression tests cannot fall through to
    /// operator configuration or the process-global vector store.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn isolated_for_test(
        route: &str,
        provider: Arc<dyn EmbeddingProvider>,
        vector_store: Arc<bbox_vectors::VectorStore>,
    ) -> Self {
        Self::from_providers(
            vec![(route.to_string(), provider, None, route.to_string())],
            Duration::from_millis(5),
            Duration::from_millis(5),
            None,
            Some(vector_store),
        )
    }

    fn start_default_with_optional_store(
        vector_store: Option<Arc<bbox_vectors::VectorStore>>,
    ) -> Self {
        match EmbeddingRouter::load_default() {
            Ok(router) => Self::from_router(
                router,
                DEFAULT_DEBOUNCE,
                DEFAULT_RETRY_BACKOFF,
                vector_store,
            ),
            Err(err) => {
                tracing::warn!(error = %err, "embedding router config failed; embedding queue disabled");
                Self::disabled_with_error(err)
            }
        }
    }

    fn from_router(
        router: EmbeddingRouter,
        debounce: Duration,
        retry_backoff: Duration,
        vector_store: Option<Arc<bbox_vectors::VectorStore>>,
    ) -> Self {
        let mut providers: Vec<ProviderSpec> = Vec::new();
        for bucket in Bucket::ALL {
            let route = bucket.as_str().to_string();
            match router.route_for(bucket, None) {
                Ok(provider) => {
                    let route_meta = match router.route(bucket, None) {
                        Ok(route_meta) => route_meta,
                        Err(err) => {
                            tracing::warn!(
                                route = %route,
                                error = %err,
                                "embedding route disabled because route metadata failed"
                            );
                            providers.push((
                                route,
                                Arc::new(FailingProvider::new(err)),
                                None,
                                String::new(),
                            ));
                            continue;
                        }
                    };
                    let vector_route = route_meta.vector_route_id();
                    let provider: Arc<dyn EmbeddingProvider> = provider.into();
                    let rate_limit_per_min = router.rate_limit_per_min(provider.id());
                    providers.push((route, provider, rate_limit_per_min, vector_route));
                }
                Err(err) => {
                    tracing::warn!(
                        route = %route,
                        error = %err,
                        "embedding route disabled because provider could not be constructed"
                    );
                    providers.push((
                        route,
                        Arc::new(FailingProvider::new(err)),
                        None,
                        String::new(),
                    ));
                }
            }
        }
        Self::from_providers(
            providers,
            debounce,
            retry_backoff,
            Some(router),
            vector_store,
        )
    }

    /// Historic `bool` API: `true` only when the request was placed on a
    /// queue. Preserved for the many hook/store callers that ignore the
    /// finer outcome; the residue sweeper uses the durable `capped_count`
    /// counter (bumped in `try_reserve_queue`) rather than this return value.
    pub fn enqueue(&self, request: EmbedRequest) -> bool {
        self.enqueue_outcome(request).enqueued()
    }

    /// Enqueue with a classified outcome so callers can distinguish a
    /// backpressure rejection (`QueueFull` — residue remains) from a dedup or
    /// empty-text skip (`Skipped` — nothing to do) and from a route/config
    /// error (`Error`). A `QueueFull` rejection is counted in the route's
    /// durable `capped_count` instead of being silently discarded
    /// (gap-7323e96c).
    pub fn enqueue_outcome(&self, request: EmbedRequest) -> EnqueueOutcome {
        let resolved = match self.resolve_route(&request) {
            Ok(resolved) => resolved,
            Err(err) => {
                // Visual requests carry a placeholder bucket (ignored by
                // routing), so attribute their failures to the visual route
                // label; marking the bucket would flag a healthy text route.
                let fallback = match &request.visual_kind {
                    Some(kind) => visual_queue_route(kind),
                    None => request.bucket.as_str().to_string(),
                };
                mark_error(&self.inner.statuses, &fallback, &sanitize_error(&err));
                return EnqueueOutcome::Error;
            }
        };
        if !embeddable_text(&request.text) {
            tracing::debug!(
                route = %resolved.queue_route,
                entity_id = %request.entity_id,
                chunk_hash = %request.chunk_hash,
                "embedding enqueue skipped empty text (providers reject empty input)"
            );
            return EnqueueOutcome::Skipped;
        }
        if !self.should_embed(&request, &resolved.vector_route) {
            tracing::debug!(
                route = %resolved.queue_route,
                vector_route = %resolved.vector_route,
                entity_id = %request.entity_id,
                chunk_hash = %request.chunk_hash,
                "embedding enqueue skipped unchanged vector record"
            );
            return EnqueueOutcome::Skipped;
        }
        let request_bytes = request.text.len() as u64;
        let max_depth = self
            .inner
            .max_queue_depth
            .load(std::sync::atomic::Ordering::Relaxed);
        if !try_reserve_queue(
            &self.inner.statuses,
            &resolved.queue_route,
            request_bytes,
            max_depth,
        ) {
            tracing::warn!(
                route = %resolved.queue_route,
                entity_id = %request.entity_id,
                request_bytes,
                "embedding enqueue rejected because route queue is full; residue remains for the sweeper"
            );
            return EnqueueOutcome::QueueFull;
        }
        let sender = self.ensure_sender(&resolved, &request);
        match sender {
            Some(sender) => {
                let sent = sender.send(WorkerCommand::Enqueue(request)).is_ok();
                if !sent {
                    release_queue(
                        &self.inner.statuses,
                        &resolved.queue_route,
                        1,
                        request_bytes,
                    );
                    mark_error(
                        &self.inner.statuses,
                        &resolved.queue_route,
                        "embedding route worker stopped",
                    );
                    return EnqueueOutcome::Error;
                }
                EnqueueOutcome::Enqueued
            }
            None => {
                release_queue(
                    &self.inner.statuses,
                    &resolved.queue_route,
                    1,
                    request_bytes,
                );
                mark_error(
                    &self.inner.statuses,
                    &resolved.queue_route,
                    "embedding route is not configured",
                );
                EnqueueOutcome::Error
            }
        }
    }

    pub fn tombstone(&self, entity_id: &str) {
        if let Some(vector_store) = &self.inner.vector_store {
            if let Err(err) = vector_store.delete_entity_all_routes(entity_id) {
                tracing::warn!(
                    entity_id,
                    error = %err,
                    "embedding tombstone failed; vector WAL can be reconstructed by reindex"
                );
            }
        }
        tracing::debug!(entity_id, "embedding tombstone accepted");
    }

    pub fn status(&self) -> EmbedStatusResponse {
        let mut response = EmbedStatusResponse {
            routes: self.inner.statuses.read().clone(),
        };
        normalize_route_statuses(&mut response);
        response
    }

    pub fn shutdown(&self) {
        for sender in self.inner.senders.read().values() {
            let _ = sender.send(WorkerCommand::Shutdown);
        }
    }

    fn resolve_route(&self, request: &EmbedRequest) -> Result<ResolvedRoute> {
        if let Some(kind) = &request.visual_kind {
            return self.resolve_visual_route(kind);
        }
        let Some(router) = &self.inner.router else {
            let route = request.bucket.as_str().to_string();
            return Ok(ResolvedRoute {
                queue_route: route.clone(),
                vector_route: route,
            });
        };
        let (queue_route, vector_route) =
            router.queue_and_vector_route(request.bucket, request.project_id.as_deref())?;
        Ok(ResolvedRoute {
            queue_route,
            vector_route,
        })
    }

    /// Visual routing is chunk-kind-keyed (`[embed.routes.visual]`), never
    /// `Bucket`-keyed, see `EmbeddingRouter::visual_route`. Mirrors
    /// `resolve_route`'s router-absent fallback: without a router, the
    /// queue and vector route both collapse to the `visual:<kind>` label
    /// (matches test scaffolding that constructs a queue with no router).
    fn resolve_visual_route(&self, kind: &str) -> Result<ResolvedRoute> {
        let queue_route = visual_queue_route(kind);
        let Some(router) = &self.inner.router else {
            return Ok(ResolvedRoute {
                queue_route: queue_route.clone(),
                vector_route: queue_route,
            });
        };
        let Some(meta) = router.visual_route(kind)? else {
            bail!(
                "visual chunk kind `{kind}` has no configured route; add \
                 [embed.routes.visual] `{kind} = \"voyage_visual\"` (or another \
                 multimodal alias) to embed.toml to opt in"
            );
        };
        Ok(ResolvedRoute {
            queue_route,
            vector_route: meta.vector_route_id(),
        })
    }

    /// Is an ACTIVE vector already stored at this `(entity_id, content_hash)`
    /// on the route this bucket resolves to?
    ///
    /// `None` means "cannot prove it": no route table, no vector store, an
    /// unresolvable route, or a probe error. Callers verifying coverage must
    /// treat `None` as uncovered and re-enqueue - enqueue is idempotent and
    /// dedup-skipped, so an unnecessary push is cheap, while trusting an
    /// unprovable "covered" would commit a lexical-only view whose vector view
    /// was promised.
    pub fn vector_is_active(
        &self,
        bucket: Bucket,
        entity_id: &str,
        content_hash: &str,
    ) -> Option<bool> {
        let vector_store = self.inner.vector_store.as_ref()?;
        self.inner.router.as_ref()?;
        let resolved = self
            .resolve_route(&EmbedRequest {
                bucket,
                project_id: None,
                entity_id: entity_id.to_string(),
                chunk_hash: content_hash.to_string(),
                text: String::new(),
                visual_kind: None,
                visual_payload: None,
            })
            .ok()?;
        vector_store
            .contains_active(&resolved.vector_route, entity_id, content_hash)
            .ok()
    }

    fn should_embed(&self, request: &EmbedRequest, vector_route: &str) -> bool {
        let Some(vector_store) = &self.inner.vector_store else {
            return true;
        };
        if self.inner.router.is_none() {
            return true;
        }
        match vector_store.contains_active(vector_route, &request.entity_id, &request.chunk_hash) {
            Ok(already_indexed) => !already_indexed,
            Err(err) => {
                tracing::warn!(
                    vector_route,
                    entity_id = %request.entity_id,
                    error = %err,
                    "embedding dedup check failed; enqueueing so vector store can recover"
                );
                true
            }
        }
    }

    fn ensure_sender(
        &self,
        resolved: &ResolvedRoute,
        request: &EmbedRequest,
    ) -> Option<mpsc::UnboundedSender<WorkerCommand>> {
        let route = resolved.queue_route.as_str();
        if let Some(sender) = self.inner.senders.read().get(route).cloned() {
            return Some(sender);
        }
        let router = self.inner.router.as_ref()?;
        let provider = match &request.visual_kind {
            Some(kind) => match router.visual_provider(kind) {
                Ok(Some(provider)) => provider,
                Ok(None) => {
                    mark_error(
                        &self.inner.statuses,
                        route,
                        &format!("visual chunk kind `{kind}` has no configured route"),
                    );
                    return None;
                }
                Err(err) => {
                    mark_error(&self.inner.statuses, route, &sanitize_error(&err));
                    return None;
                }
            },
            None => match router.route_for(request.bucket, request.project_id.as_deref()) {
                Ok(provider) => provider,
                Err(err) => {
                    mark_error(&self.inner.statuses, route, &sanitize_error(&err));
                    return None;
                }
            },
        };
        let provider: Arc<dyn EmbeddingProvider> = provider.into();
        let rate_limit_per_min = router.rate_limit_per_min(provider.id());
        let status = provider_route_status(provider.as_ref());
        let (tx, rx) = mpsc::unbounded_channel();
        self.inner
            .statuses
            .write()
            .entry(route.to_string())
            .or_insert(status);
        let spec = WorkerSpec {
            route: route.to_string(),
            vector_route: resolved.vector_route.clone(),
            provider,
            rate_limit_per_min,
            debounce: self.inner.debounce,
            retry_backoff: self.inner.retry_backoff,
            statuses: self.inner.statuses.clone(),
            vector_store: self.inner.vector_store.clone(),
            persist_vectors: true,
        };
        spawn_worker_loop(route, spec, rx);
        self.inner
            .senders
            .write()
            .insert(route.to_string(), tx.clone());
        Some(tx)
    }

    fn disabled_with_error(err: anyhow::Error) -> Self {
        let mut statuses = BTreeMap::new();
        let message = sanitize_error(&err);
        for bucket in Bucket::ALL {
            statuses.insert(
                bucket.as_str().to_string(),
                RouteStatus {
                    available: false,
                    health: "unavailable".into(),
                    health_reason: Some(classify_error_reason(&message).into()),
                    last_error: Some(message.clone()),
                    ..RouteStatus::default()
                },
            );
        }
        Self {
            inner: Arc::new(EmbedQueueInner {
                senders: RwLock::new(BTreeMap::new()),
                statuses: Arc::new(RwLock::new(statuses)),
                router: None,
                vector_store: None,
                debounce: DEFAULT_DEBOUNCE,
                retry_backoff: DEFAULT_RETRY_BACKOFF,
                max_queue_depth: std::sync::atomic::AtomicU64::new(MAX_ROUTE_QUEUE_DEPTH),
            }),
        }
    }

    #[cfg(test)]
    fn from_providers_for_test(
        providers: Vec<(&str, Arc<dyn EmbeddingProvider>)>,
        debounce: Duration,
        retry_backoff: Duration,
    ) -> Self {
        Self::from_providers(
            providers
                .into_iter()
                .map(|(route, provider)| (route.to_string(), provider, None, route.to_string()))
                .collect(),
            debounce,
            retry_backoff,
            None,
            None,
        )
    }

    #[cfg(test)]
    fn from_router_for_test(
        router: EmbeddingRouter,
        debounce: Duration,
        retry_backoff: Duration,
    ) -> Self {
        Self::from_providers(Vec::new(), debounce, retry_backoff, Some(router), None)
    }

    fn from_providers(
        providers: Vec<ProviderSpec>,
        debounce: Duration,
        retry_backoff: Duration,
        router: Option<EmbeddingRouter>,
        vector_store: Option<Arc<bbox_vectors::VectorStore>>,
    ) -> Self {
        let statuses = Arc::new(RwLock::new(BTreeMap::new()));
        let mut senders = BTreeMap::new();
        for (route, provider, rate_limit_per_min, vector_route) in providers {
            let status = provider_route_status(provider.as_ref());
            statuses.write().entry(route.clone()).or_insert(status);
            let (tx, rx) = mpsc::unbounded_channel();
            let spec = WorkerSpec {
                route: route.clone(),
                vector_route,
                provider,
                rate_limit_per_min,
                debounce,
                retry_backoff,
                statuses: statuses.clone(),
                vector_store: vector_store.clone(),
                persist_vectors: vector_store.is_some(),
            };
            spawn_worker_loop(&route, spec, rx);
            senders.insert(route, tx);
        }
        Self {
            inner: Arc::new(EmbedQueueInner {
                senders: RwLock::new(senders),
                statuses,
                router,
                vector_store,
                debounce,
                retry_backoff,
                max_queue_depth: std::sync::atomic::AtomicU64::new(MAX_ROUTE_QUEUE_DEPTH),
            }),
        }
    }

    /// Lower the per-route depth cap so convergence tests exercise the
    /// cap→drain→refill cycle with a handful of items instead of 10k.
    #[cfg(test)]
    fn set_max_queue_depth(&self, depth: u64) {
        self.inner
            .max_queue_depth
            .store(depth, std::sync::atomic::Ordering::Relaxed);
    }
}

fn spawn_worker_loop(route: &str, spec: WorkerSpec, rx: mpsc::UnboundedReceiver<WorkerCommand>) {
    if let Ok(handle) = tokio::runtime::Handle::try_current() {
        handle.spawn(worker_loop(spec, rx));
        return;
    }

    let route_for_thread = route.to_string();
    let thread_name = format!("embed-worker-{route_for_thread}");
    let spawn_result = std::thread::Builder::new()
        .name(thread_name)
        .spawn(move || {
            match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(worker_loop(spec, rx)),
                Err(err) => tracing::error!(
                    route = %route_for_thread,
                    error = %err,
                    "embedding worker runtime failed to initialize"
                ),
            }
        });

    if let Err(err) = spawn_result {
        tracing::error!(
            route = %route,
            error = %err,
            "embedding worker thread failed to start"
        );
    }
}

fn pack_mode_for(endpoint_kind: EmbedEndpointKind) -> PackMode {
    match endpoint_kind {
        EmbedEndpointKind::ContextualizedText => PackMode::Grouped,
        EmbedEndpointKind::Multimodal => PackMode::Visual,
        EmbedEndpointKind::Text | EmbedEndpointKind::Ollama => PackMode::Flat,
    }
}

async fn worker_loop(spec: WorkerSpec, mut rx: mpsc::UnboundedReceiver<WorkerCommand>) {
    let mut pending = VecDeque::new();
    let mut retry_batch = Vec::new();
    let mut retry_attempts = 0_u8;
    let mut backoff = spec.retry_backoff;
    let mut rate_limiter = spec.rate_limit_per_min.and_then(TokenBucket::new);
    loop {
        // Drain up to WORKER_CONCURRENCY batches and dispatch them in
        // parallel. The retry path keeps single-batch semantics so we
        // don't fan out a known-failing batch.
        let batches = if !retry_batch.is_empty() {
            vec![retry_batch.clone()]
        } else {
            let mut acc = Vec::with_capacity(WORKER_CONCURRENCY);
            // First batch blocks for input; subsequent batches only
            // contribute if pending has more work right away (so we
            // don't add latency waiting for a second batch to fill).
            let mode = pack_mode_for(spec.provider.endpoint_kind());
            match collect_quiescent_batch(&mut rx, &mut pending, spec.debounce, mode).await {
                Some(b) => acc.push(b),
                None => return,
            }
            for _ in 1..WORKER_CONCURRENCY {
                if pending.is_empty() {
                    break;
                }
                let batch = pack_batch(&mut pending, mode);
                if !batch.is_empty() {
                    acc.push(batch);
                }
            }
            acc
        };
        if let Some(limiter) = &mut rate_limiter {
            for batch in batches {
                limiter.acquire(1).await;
                let result = embed_batch_requests(spec.provider.as_ref(), &batch).await;
                process_batch_outcome(
                    &spec,
                    &mut retry_batch,
                    &mut retry_attempts,
                    &mut backoff,
                    batch,
                    result,
                )
                .await;
            }
            continue;
        }
        // Dispatch all unthrottled batches concurrently. The rate-limited
        // path sends each batch as soon as its permit is available; otherwise
        // a worker can look stuck while it waits for permits for later batches.
        let provider = &spec.provider;
        let mut results: Vec<(Vec<EmbedRequest>, anyhow::Result<Vec<Vec<f32>>>)> = {
            let futures = batches
                .iter()
                .map(|batch| async move { embed_batch_requests(provider.as_ref(), batch).await })
                .collect::<Vec<_>>();
            let outcomes = futures::future::join_all(futures).await;
            batches.into_iter().zip(outcomes).collect()
        };
        // Process results sequentially (persist + retry-or-drop is not
        // safe to run in parallel against the same WAL). Each batch is
        // treated independently for retry decisions; we still mutate
        // the worker-level retry/backoff state so a persistent failure
        // backs off the route as a whole.
        for (batch, result) in results.drain(..) {
            process_batch_outcome(
                &spec,
                &mut retry_batch,
                &mut retry_attempts,
                &mut backoff,
                batch,
                result,
            )
            .await;
        }
    }
}

/// Per-batch outcome handler. Persists vectors on success, schedules
/// retry/drop on failure. Mutates retry/backoff state on the worker so
/// a sticky provider outage backs off the whole route.
async fn process_batch_outcome(
    spec: &WorkerSpec,
    retry_batch: &mut Vec<EmbedRequest>,
    retry_attempts: &mut u8,
    backoff: &mut Duration,
    batch: Vec<EmbedRequest>,
    result: anyhow::Result<Vec<Vec<f32>>>,
) {
    match result {
        Ok(vectors) => {
            if spec.persist_vectors {
                if let Err(err) = persist_vectors(spec, &batch, vectors) {
                    let sanitized = sanitize_error(&err);
                    tracing::warn!(
                        route = %spec.route,
                        vector_route = %spec.vector_route,
                        error = %sanitized,
                        "embedding vector persistence failed; route will retry"
                    );
                    if !schedule_retry_or_drop(
                        spec,
                        retry_batch,
                        retry_attempts,
                        backoff,
                        batch,
                        &sanitized,
                    )
                    .await
                    {
                        *backoff = spec.retry_backoff;
                    }
                    return;
                }
            }
            tracing::debug!(
                route = %spec.route,
                vector_route = %spec.vector_route,
                vectors = batch.len(),
                dimensions = spec.provider.dimensions(),
                model = spec.provider.document_model(),
                "embedding vectors persisted"
            );
            mark_success(
                &spec.statuses,
                &spec.route,
                batch.len() as u64,
                batch_text_bytes(&batch),
            );
            retry_batch.clear();
            *retry_attempts = 0;
            *backoff = spec.retry_backoff;
        }
        Err(err) => {
            if is_non_retryable(&err) {
                // Retrying an identical payload rejection fails identically;
                // bisect to the poison item(s) instead of dropping the batch.
                tracing::warn!(
                    route = %spec.route,
                    batch = batch.len(),
                    error = %sanitize_error(&err),
                    "embedding batch rejected by provider; isolating poison payload"
                );
                isolate_poison_batch(spec, batch).await;
                retry_batch.clear();
                *retry_attempts = 0;
                *backoff = spec.retry_backoff;
                return;
            }
            let sanitized = sanitize_error(&err);
            tracing::warn!(
                route = %spec.route,
                error = %sanitized,
                "embedding batch failed; route will retry without affecting search"
            );
            if !schedule_retry_or_drop(
                spec,
                retry_batch,
                retry_attempts,
                backoff,
                batch,
                &sanitized,
            )
            .await
            {
                *backoff = spec.retry_backoff;
            }
        }
    }
}

/// Bisect a non-retryably-rejected batch down to the poison item(s):
/// re-embed the halves independently so one provider-rejected payload
/// (e.g. an empty string Voyage 400s on) cannot strand its batch-mates
/// (gap-e3e033ce). Poison items are dropped individually and counted in
/// the route's durable `dropped_count` with the entity_id in
/// `last_dropped`; the route stays available (the batch-mates embedded,
/// so the route is healthy). Transient errors during isolation re-try the
/// same sub-batch; the call budget bounds the whole pass (~2x batch size
/// worst case) so a flapping provider cannot wedge the worker here.
async fn isolate_poison_batch(spec: &WorkerSpec, batch: Vec<EmbedRequest>) {
    let mut stack = vec![batch];
    let mut budget: usize = 2 * MAX_BATCH_DOCS + 16;
    while let Some(batch) = stack.pop() {
        if batch.is_empty() {
            continue;
        }
        if budget == 0 {
            let message = format!(
                "embedding poison isolation call budget exhausted; dropping {} item(s)",
                batch.len()
            );
            tracing::warn!(route = %spec.route, dropped = batch.len(), "{message}");
            mark_poison_dropped(
                &spec.statuses,
                &spec.route,
                batch.len() as u64,
                batch_text_bytes(&batch),
                &message,
            );
            continue;
        }
        budget -= 1;
        match embed_batch_requests(spec.provider.as_ref(), &batch).await {
            Ok(vectors) => {
                if spec.persist_vectors {
                    if let Err(err) = persist_vectors(spec, &batch, vectors) {
                        let sanitized = sanitize_error(&err);
                        let message = format!(
                            "vector persistence failed during poison isolation: {sanitized}"
                        );
                        tracing::warn!(route = %spec.route, error = %sanitized, "{message}");
                        mark_poison_dropped(
                            &spec.statuses,
                            &spec.route,
                            batch.len() as u64,
                            batch_text_bytes(&batch),
                            &message,
                        );
                        continue;
                    }
                }
                mark_success(
                    &spec.statuses,
                    &spec.route,
                    batch.len() as u64,
                    batch_text_bytes(&batch),
                );
            }
            Err(err) => {
                let non_retryable = is_non_retryable(&err);
                if non_retryable && batch.len() == 1 {
                    let req = &batch[0];
                    let sanitized = sanitize_error(&err);
                    let message = format!(
                        "embedding item dropped as poison payload: entity_id={} chunk_hash={} text_bytes={}: {sanitized}",
                        req.entity_id,
                        req.chunk_hash,
                        req.text.len()
                    );
                    tracing::warn!(
                        route = %spec.route,
                        entity_id = %req.entity_id,
                        chunk_hash = %req.chunk_hash,
                        error = %sanitized,
                        "poison embedding payload dropped"
                    );
                    mark_poison_dropped(
                        &spec.statuses,
                        &spec.route,
                        1,
                        batch_text_bytes(&batch),
                        &message,
                    );
                } else if non_retryable {
                    let mut left = batch;
                    let right = left.split_off(left.len() / 2);
                    stack.push(left);
                    stack.push(right);
                } else {
                    // Transient failure mid-isolation: brief backoff and
                    // retry the same sub-batch; the budget bounds total work.
                    mark_retry(&spec.statuses, &spec.route);
                    tokio::time::sleep(spec.retry_backoff).await;
                    stack.push(batch);
                }
            }
        }
    }
}

async fn schedule_retry_or_drop(
    spec: &WorkerSpec,
    retry_batch: &mut Vec<EmbedRequest>,
    retry_attempts: &mut u8,
    backoff: &mut Duration,
    batch: Vec<EmbedRequest>,
    error: &str,
) -> bool {
    *retry_attempts = retry_attempts.saturating_add(1);
    mark_retry(&spec.statuses, &spec.route);
    if *retry_attempts >= MAX_BATCH_RETRIES {
        let dropped = batch.len() as u64;
        let dropped_bytes = batch_text_bytes(&batch);
        let message = format!("embedding batch dropped after {MAX_BATCH_RETRIES} retries: {error}");
        tracing::warn!(
            route = %spec.route,
            vector_route = %spec.vector_route,
            dropped,
            "embedding batch dropped after retry limit"
        );
        mark_dropped(
            &spec.statuses,
            &spec.route,
            dropped,
            dropped_bytes,
            &message,
        );
        retry_batch.clear();
        *retry_attempts = 0;
        false
    } else {
        retry_batch.clear();
        retry_batch.extend(batch);
        tokio::time::sleep(*backoff).await;
        *backoff = (*backoff * 2).min(MAX_RETRY_BACKOFF);
        true
    }
}

// Voyage API caps: 128 docs/request and ~120k tokens/request. The bytes
// cap is the worst-case-token guard: voyage tokenizes raw text, and in the
// pathological case (single-char tokens — backticks, hyphens, code-fence
// boundaries dominate markdown chunks) the ratio is ~1 char / 1 token. So
// to stay under 120k tokens we cap at ~100 KB of input. A previous
// 200 KB cap accepted 16-doc batches at 168k tokens and ate voyage 400s.
//
// Document count is capped at voyage's hard 128 limit; the bytes guard
// usually triggers first on dense text.
/// Grouped (contextualized) packing only: one document window must stay
/// inside the contextual model's own context window (32K tokens for
/// voyage-context-4, which rejects rather than truncates). 64 KiB of text
/// is safely inside even at dense-code token ratios.
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_BATCH_DOCS: usize = 128;
const MAX_BATCH_BYTES: usize = 100 * 1024;
// Visual (multimodal) packing: images are heavy per item (up to the
// provider's 20 MB single-image cap, design/corpus/agentic-corpus/
// multimodal-embedding-routing.md) and the request's `text` is just a
// filename-stem caption, so the text-byte heuristic above does not reflect
// actual request weight. Cap the batch far below Voyage's 1,000-input/
// 320K-token multimodal ceiling: a handful of large images is already a
// multi-ten-MB HTTP body, and there is no benefit to batching harder than
// the debounce window already buys.
const MAX_VISUAL_BATCH_DOCS: usize = 8;
const MAX_VISUAL_BATCH_BYTES: usize = 64 * 1024 * 1024;
// Per-worker concurrency: we dispatch multiple full batches in parallel
// before awaiting any of them. With 4 in-flight batches at ~1.5s each =
// ~2.7 batches/sec/worker = ~162 RPM/worker. Across 6 routes that's below
// Voyage's 2000 RPM ceiling; MAX_BATCH_BYTES is the separate TPM guard.
// If you raise this, also consider routes that share the same provider+model
// partition to avoid lock contention on persist_vectors.
const WORKER_CONCURRENCY: usize = 4;

async fn collect_quiescent_batch(
    rx: &mut mpsc::UnboundedReceiver<WorkerCommand>,
    pending: &mut VecDeque<EmbedRequest>,
    debounce: Duration,
    mode: PackMode,
) -> Option<Vec<EmbedRequest>> {
    while pending.is_empty() {
        match rx.recv().await? {
            WorkerCommand::Enqueue(request) => pending.push_back(request),
            WorkerCommand::Shutdown => return None,
        }
    }
    loop {
        match tokio::time::timeout(debounce, rx.recv()).await {
            Ok(Some(WorkerCommand::Enqueue(request))) => pending.push_back(request),
            Ok(Some(WorkerCommand::Shutdown)) | Ok(None) => return None,
            Err(_) => break,
        }
    }
    Some(pack_batch(pending, mode))
}

fn persist_vectors(
    spec: &WorkerSpec,
    batch: &[EmbedRequest],
    vectors: Vec<Vec<f32>>,
) -> Result<()> {
    if vectors.len() != batch.len() {
        return Err(anyhow!(
            "provider returned {} vectors for {} requests",
            vectors.len(),
            batch.len()
        ));
    }
    let mut contradiction_checks = Vec::new();
    let records = batch
        .iter()
        .zip(vectors)
        .map(|(request, vector)| {
            // `bucket` is an ignored placeholder on visual requests
            // (routing goes through `visual_kind` instead), so gate on it
            // explicitly rather than relying on the placeholder value never
            // colliding with `Bucket::Knowledge`.
            if request.visual_kind.is_none() && request.bucket == Bucket::Knowledge {
                contradiction_checks.push((request.clone(), vector.clone()));
            }
            bbox_vectors::VectorUpsert {
                entity_id: request.entity_id.clone(),
                content_hash: request.chunk_hash.clone(),
                vector,
            }
        })
        .collect();
    let Some(store) = &spec.vector_store else {
        return Ok(());
    };
    store.upsert_batch(&spec.vector_route, records)?;
    for (request, vector) in contradiction_checks {
        if let Some(hook) = CONTRADICTION_HOOK.get() {
            hook(&request, &spec.vector_route, &vector);
        }
    }
    Ok(())
}

struct TokenBucket {
    capacity: f64,
    tokens: f64,
    refill_per_second: f64,
    last_refill: Instant,
}

impl TokenBucket {
    fn new(rate_limit_per_min: u32) -> Option<Self> {
        if rate_limit_per_min == 0 {
            return None;
        }
        let capacity = f64::from(rate_limit_per_min);
        Some(Self {
            capacity,
            tokens: capacity,
            refill_per_second: capacity / 60.0,
            last_refill: Instant::now(),
        })
    }

    async fn acquire(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let needed = (count as f64).min(self.capacity);
        loop {
            self.refill();
            if self.tokens >= needed {
                self.tokens -= needed;
                return;
            }
            let missing = needed - self.tokens;
            let wait = Duration::from_secs_f64(missing / self.refill_per_second);
            tokio::time::sleep(wait).await;
        }
    }

    fn refill(&mut self) {
        let elapsed = self.last_refill.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        self.tokens = (self.tokens + elapsed * self.refill_per_second).min(self.capacity);
        self.last_refill = Instant::now();
    }

    #[cfg(test)]
    fn try_take_now(&mut self, count: usize) -> bool {
        self.refill();
        let needed = (count as f64).min(self.capacity);
        if self.tokens >= needed {
            self.tokens -= needed;
            true
        } else {
            false
        }
    }
}

fn try_reserve_queue(
    statuses: &RwLock<BTreeMap<String, RouteStatus>>,
    route: &str,
    bytes: u64,
    max_depth: u64,
) -> bool {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    if status.queue_depth >= max_depth
        || status.queue_bytes.saturating_add(bytes) > MAX_ROUTE_QUEUE_BYTES
    {
        status.available = false;
        status.health = "unavailable".into();
        status.health_reason = Some("queue_full".into());
        status.last_error = Some(format!(
            "embedding route queue full: depth={} bytes={} max_depth={} max_bytes={}",
            status.queue_depth, status.queue_bytes, max_depth, MAX_ROUTE_QUEUE_BYTES
        ));
        // Count the rejection so the sweeper (and bbox_embed_status) can see
        // "capped, residue remains" instead of a silently dropped item.
        status.capped_count = status.capped_count.saturating_add(1);
        return false;
    }
    status.queue_depth = status.queue_depth.saturating_add(1);
    status.queue_bytes = status.queue_bytes.saturating_add(bytes);
    true
}

fn release_queue(
    statuses: &RwLock<BTreeMap<String, RouteStatus>>,
    route: &str,
    count: u64,
    bytes: u64,
) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.queue_depth = status.queue_depth.saturating_sub(count);
    status.queue_bytes = status.queue_bytes.saturating_sub(bytes);
}

fn mark_success(
    statuses: &RwLock<BTreeMap<String, RouteStatus>>,
    route: &str,
    count: u64,
    bytes: u64,
) {
    let drained = {
        let mut statuses = statuses.write();
        let status = statuses.entry(route.to_string()).or_default();
        status.available = true;
        status.health = "ok".into();
        status.indexed_count = status.indexed_count.saturating_add(count);
        status.queue_depth = status.queue_depth.saturating_sub(count);
        status.queue_bytes = status.queue_bytes.saturating_sub(bytes);
        status.health_reason = None;
        status.last_error = None;
        status.queue_depth == 0
    };
    // A wave finished draining: nudge the residue sweeper (if installed) to
    // refill promptly rather than waiting for its next timer tick. Fired
    // after the lock is released so the hook never contends on `statuses`.
    if drained {
        if let Some(hook) = QUEUE_DRAIN_HOOK.get() {
            hook(route);
        }
    }
}

fn mark_retry(statuses: &RwLock<BTreeMap<String, RouteStatus>>, route: &str) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.retried_count = status.retried_count.saturating_add(1);
}

fn mark_dropped(
    statuses: &RwLock<BTreeMap<String, RouteStatus>>,
    route: &str,
    count: u64,
    bytes: u64,
    message: &str,
) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.available = false;
    status.queue_depth = status.queue_depth.saturating_sub(count);
    status.queue_bytes = status.queue_bytes.saturating_sub(bytes);
    status.health = "unavailable".into();
    status.health_reason = Some(classify_error_reason(message).into());
    status.last_error = Some(message.to_string());
    status.dropped_count = status.dropped_count.saturating_add(count);
    status.last_dropped = Some(message.to_string());
}

/// Drop poison items isolated out of an otherwise-healthy batch
/// (gap-e3e033ce). Unlike `mark_dropped` this does NOT flip the route
/// unavailable: we proved the route works by embedding the batch-mates,
/// so the only honest signal is the durable `dropped_count`/`last_dropped`
/// pair — not a route-wide outage. The diagnostic is recorded on the
/// durable fields so a later success cannot wipe it.
fn mark_poison_dropped(
    statuses: &RwLock<BTreeMap<String, RouteStatus>>,
    route: &str,
    count: u64,
    bytes: u64,
    message: &str,
) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.queue_depth = status.queue_depth.saturating_sub(count);
    status.queue_bytes = status.queue_bytes.saturating_sub(bytes);
    status.dropped_count = status.dropped_count.saturating_add(count);
    status.last_dropped = Some(message.to_string());
}

fn mark_error(statuses: &RwLock<BTreeMap<String, RouteStatus>>, route: &str, message: &str) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.available = false;
    status.health = "unavailable".into();
    status.health_reason = Some(classify_error_reason(message).into());
    status.last_error = Some(message.to_string());
}

pub fn normalize_route_statuses(response: &mut EmbedStatusResponse) {
    for status in response.routes.values_mut() {
        normalize_route_status(status);
    }
}

fn normalize_route_status(status: &mut RouteStatus) {
    if status.available {
        status.health = "ok".into();
        status.health_reason = None;
        return;
    }
    status.health = "unavailable".into();
    let reason = status
        .last_error
        .as_deref()
        .map(classify_error_reason)
        .unwrap_or("unavailable");
    status.health_reason = Some(reason.into());
}

fn classify_error_reason(message: &str) -> &'static str {
    let lower = message.to_ascii_lowercase();
    if lower.contains("queue full") {
        "queue_full"
    } else if message.contains("VOYAGE_API_KEY")
        || message.contains("DAYSTROM_VOYAGE_API_KEY")
        || lower.contains("api key")
        || lower.contains("token")
        || lower.contains("credential")
    {
        "credential_missing"
    } else if lower.contains("worker stopped") {
        "worker_stopped"
    } else if lower.contains("not configured") || lower.contains("config") {
        "not_configured"
    } else if lower.contains("unavailable") {
        "provider_unavailable"
    } else {
        "error"
    }
}

fn sanitize_error(err: &anyhow::Error) -> String {
    let mut message = err.to_string();
    if let Some((first, _)) = message.split_once('\n') {
        message = first.to_string();
    }
    if message.len() > 200 {
        message.truncate(197);
        message.push_str("...");
    }
    message
}

fn batch_text_bytes(batch: &[EmbedRequest]) -> u64 {
    batch.iter().map(|request| request.text.len() as u64).sum()
}

struct FailingProvider {
    error: String,
}

impl FailingProvider {
    fn new(err: anyhow::Error) -> Self {
        Self {
            error: sanitize_error(&err),
        }
    }
}

#[async_trait]
impl EmbeddingProvider for FailingProvider {
    async fn embed_batch(
        &self,
        _inputs: &[EmbedInput],
        _input_type: EmbedInputType,
    ) -> Result<Vec<EmbedOutput>> {
        Err(anyhow!(self.error.clone()))
    }

    fn dimensions(&self) -> usize {
        0
    }

    fn document_model(&self) -> &str {
        "unavailable"
    }

    fn endpoint_kind(&self) -> EmbedEndpointKind {
        EmbedEndpointKind::Text
    }

    fn id(&self) -> &str {
        "unavailable"
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    struct MockProvider {
        calls: AtomicUsize,
        fail: bool,
    }

    impl MockProvider {
        fn ok() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: false,
            }
        }

        fn failing() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                fail: true,
            }
        }
    }

    #[async_trait]
    impl EmbeddingProvider for MockProvider {
        async fn embed_batch(
            &self,
            inputs: &[EmbedInput],
            _input_type: EmbedInputType,
        ) -> Result<Vec<EmbedOutput>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(anyhow!("provider unavailable: token redacted"));
            }
            Ok(inputs
                .iter()
                .map(|_| EmbedOutput::single(vec![0.0_f32; 4]))
                .collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn document_model(&self) -> &str {
            "mock"
        }

        fn endpoint_kind(&self) -> EmbedEndpointKind {
            EmbedEndpointKind::Text
        }

        fn id(&self) -> &str {
            "mock"
        }
    }

    #[test]
    fn provider_workers_can_start_without_current_tokio_runtime() {
        let provider = Arc::new(MockProvider::ok());
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("knowledge", provider.clone())],
            Duration::from_millis(10),
            Duration::from_millis(10),
        );

        assert!(queue.enqueue(request(Bucket::Knowledge, "a", "h1")));
        std::thread::sleep(Duration::from_millis(80));

        let status = queue.status().routes["knowledge"].clone();
        assert!(status.available);
        assert_eq!(status.indexed_count, 1);
        assert_eq!(status.queue_depth, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        queue.shutdown();
    }

    #[tokio::test]
    async fn debounce_batches_requests() {
        let provider = Arc::new(MockProvider::ok());
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("knowledge", provider.clone())],
            Duration::from_millis(20),
            Duration::from_millis(20),
        );
        assert!(queue.enqueue(request(Bucket::Knowledge, "a", "h1")));
        assert!(queue.enqueue(request(Bucket::Knowledge, "b", "h1")));
        assert!(queue.enqueue(request(Bucket::Knowledge, "c", "h2")));
        tokio::time::sleep(Duration::from_millis(80)).await;
        let status = queue.status().routes["knowledge"].clone();
        assert!(status.available);
        assert_eq!(status.indexed_count, 3);
        assert_eq!(status.queue_depth, 0);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        queue.shutdown();
    }

    /// The queue-full path must reject cleanly and COUNT the rejection
    /// (`capped_count`) rather than silently discard the item — the
    /// silent-drop-with-no-signal semantics that stranded residue forever
    /// (gap-7323e96c). The depth cap is injectable so this runs with a cap of
    /// 2 instead of 10k.
    #[test]
    fn try_reserve_queue_caps_and_counts_rejections_instead_of_silent_drop() {
        let statuses = RwLock::new(BTreeMap::new());
        assert!(try_reserve_queue(&statuses, "code", 10, 2), "first fits");
        assert!(try_reserve_queue(&statuses, "code", 10, 2), "second fits");
        assert!(
            !try_reserve_queue(&statuses, "code", 10, 2),
            "third is capped, residue remains"
        );
        assert!(!try_reserve_queue(&statuses, "code", 10, 2), "still capped");
        let statuses = statuses.read();
        let status = &statuses["code"];
        assert_eq!(status.queue_depth, 2, "cap holds the depth at 2");
        assert_eq!(
            status.capped_count, 2,
            "each rejection is counted, not silently dropped"
        );
        assert_eq!(status.health_reason.as_deref(), Some("queue_full"));
    }

    /// Idempotence / cost safety: an enqueue whose (entity, chunk_hash) is
    /// already in the vector store returns `Skipped` and never reaches the
    /// provider. A residue sweep that re-scans a converged corpus must cost
    /// zero Voyage calls (gap-7323e96c cost-safety invariant).
    #[tokio::test]
    async fn enqueue_outcome_skips_already_embedded_items() {
        let dir = tempfile::tempdir().unwrap();
        let router = EmbeddingRouter::from_toml_str(
            r#"
            [embed.routes]
            code = "voyage"
            "#,
        )
        .unwrap();
        let vector_route = router.route(Bucket::Code, None).unwrap().vector_route_id();
        let store = Arc::new(bbox_vectors::VectorStore::open(dir.path()).unwrap());
        store
            .upsert(
                &vector_route,
                "project_file:p:f:h:0",
                "h1",
                vec![0.0_f32; 4],
            )
            .unwrap();
        let provider = Arc::new(MockProvider::ok());
        let queue = EmbedQueueHandle::from_providers(
            vec![(
                "code".to_string(),
                provider.clone() as Arc<dyn EmbeddingProvider>,
                None,
                vector_route.clone(),
            )],
            Duration::from_millis(10),
            Duration::from_millis(10),
            Some(router),
            Some(store),
        );
        let req = request(Bucket::Code, "project_file:p:f:h:0", "h1");
        assert_eq!(queue.enqueue_outcome(req), EnqueueOutcome::Skipped);
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "a dedup-skipped enqueue must not call the provider"
        );
        queue.shutdown();
    }

    /// One provider-rejected payload must not strand its batch-mates
    /// (gap-e3e033ce): the worker bisects the non-retryable batch, embeds
    /// the good items, and drops only the poison item with its entity_id
    /// named in last_error.
    #[tokio::test]
    async fn poison_payload_is_isolated_and_batch_mates_embed() {
        struct PoisonProvider {
            calls: AtomicUsize,
        }

        #[async_trait]
        impl EmbeddingProvider for PoisonProvider {
            async fn embed_batch(
                &self,
                inputs: &[EmbedInput],
                _input_type: EmbedInputType,
            ) -> Result<Vec<EmbedOutput>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                if inputs
                    .iter()
                    .any(|input| matches!(input, EmbedInput::Text(text) if text == "POISON"))
                {
                    return Err(anyhow::Error::new(NonRetryableBatchError)
                        .context("voyage embedding request failed: HTTP 400 body=Input cannot contain empty strings"));
                }
                Ok(inputs
                    .iter()
                    .map(|_| EmbedOutput::single(vec![0.0_f32; 4]))
                    .collect())
            }

            fn dimensions(&self) -> usize {
                4
            }

            fn document_model(&self) -> &str {
                "mock"
            }

            fn endpoint_kind(&self) -> EmbedEndpointKind {
                EmbedEndpointKind::Text
            }

            fn id(&self) -> &str {
                "mock"
            }
        }

        let provider = Arc::new(PoisonProvider {
            calls: AtomicUsize::new(0),
        });
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("code", provider.clone())],
            Duration::from_millis(20),
            Duration::from_millis(10),
        );
        assert!(queue.enqueue(request_with_text(Bucket::Code, "good-a", "h1", "fn a() {}")));
        assert!(queue.enqueue(request_with_text(Bucket::Code, "bad-b", "h2", "POISON")));
        assert!(queue.enqueue(request_with_text(Bucket::Code, "good-c", "h3", "fn c() {}")));
        tokio::time::sleep(Duration::from_millis(200)).await;

        let status = queue.status().routes["code"].clone();
        assert_eq!(status.indexed_count, 2, "both good items embed: {status:?}");
        assert_eq!(status.queue_depth, 0);
        // The poison drop is recorded on the DURABLE fields, which a later
        // mark_success (the good batch-mate embedding after the drop) does
        // not clear — that clobbering of last_error was the original bug.
        assert_eq!(status.dropped_count, 1, "exactly the poison item dropped");
        let last_dropped = status
            .last_dropped
            .expect("poison drop records last_dropped");
        assert!(
            last_dropped.contains("poison payload") && last_dropped.contains("bad-b"),
            "drop diagnostic names the poison entity: {last_dropped}"
        );
        // Route is healthy — the batch-mates proved it works; a single
        // poison item must not flip the whole route unavailable.
        assert!(
            status.available,
            "poison isolation keeps the route available"
        );
        queue.shutdown();
    }

    /// Residue larger than the queue cap converges across refill waves: fill
    /// to the cap, drain, refill the remainder — the loop the residue sweeper
    /// drives (gap-7323e96c). The cap rejection is a clean `QueueFull`
    /// signal, never a silent drop, so no item is lost.
    #[tokio::test]
    async fn residue_beyond_cap_converges_across_refill_waves() {
        let provider = Arc::new(MockProvider::ok());
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("code", provider.clone())],
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        queue.set_max_queue_depth(3);
        let total = 10usize;
        // `accepted` stands in for the sweeper's store-backed dedup (this
        // test harness has no vector store, so the queue can't dedup itself).
        let mut accepted = std::collections::HashSet::new();
        let mut hit_cap = false;
        for _wave in 0..12 {
            for i in 0..total {
                let id = format!("e{i}");
                if accepted.contains(&id) {
                    continue;
                }
                match queue.enqueue_outcome(request(Bucket::Code, &id, "h1")) {
                    EnqueueOutcome::Enqueued => {
                        accepted.insert(id);
                    }
                    EnqueueOutcome::QueueFull => {
                        // Capped: residue remains — the sweeper comes back
                        // after the wave drains.
                        hit_cap = true;
                        break;
                    }
                    other => panic!("unexpected enqueue outcome: {other:?}"),
                }
            }
            if accepted.len() == total {
                break;
            }
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        assert!(
            hit_cap,
            "a 10-item residue over a cap of 3 must hit the cap"
        );
        assert_eq!(accepted.len(), total, "every residue item enqueued");
        // Let the final wave drain before asserting embedded counts.
        for _ in 0..30 {
            if queue.status().routes["code"].indexed_count == total as u64 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(30)).await;
        }
        let status = queue.status().routes["code"].clone();
        assert_eq!(status.indexed_count, total as u64);
        assert_eq!(status.queue_depth, 0);
        assert!(
            status.capped_count >= 1,
            "cap rejections are counted, not silently dropped"
        );
        queue.shutdown();
    }

    /// Empty text never reaches the provider — Voyage rejects it with an
    /// HTTP 400 that would poison the whole batch (gap-e3e033ce).
    #[tokio::test]
    async fn enqueue_skips_empty_text() {
        let provider = Arc::new(MockProvider::ok());
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("code", provider.clone())],
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        assert!(!queue.enqueue(request_with_text(Bucket::Code, "empty", "h1", "")));
        assert!(!queue.enqueue(request_with_text(Bucket::Code, "blank", "h2", "  \n\t")));
        let status = queue.status().routes["code"].clone();
        assert_eq!(status.queue_depth, 0);
        assert_eq!(status.queue_bytes, 0);
        queue.shutdown();
    }

    #[tokio::test]
    async fn provider_outage_drops_batch_after_retry_limit() {
        let queue = EmbedQueueHandle::from_providers_for_test(
            vec![("code", Arc::new(MockProvider::failing()))],
            Duration::from_millis(10),
            Duration::from_millis(10),
        );
        assert!(queue.enqueue(request(Bucket::Code, "a", "h1")));
        tokio::time::sleep(Duration::from_millis(120)).await;
        let status = queue.status().routes["code"].clone();
        assert!(!status.available);
        assert_eq!(status.queue_depth, 0);
        assert_eq!(status.retried_count, u64::from(MAX_BATCH_RETRIES));
        assert!(
            status
                .last_error
                .unwrap()
                .contains("dropped after 3 retries")
        );
        queue.shutdown();
    }

    #[test]
    fn route_health_classifies_common_failure_reasons() {
        let mut response = EmbedStatusResponse {
            routes: BTreeMap::from([
                (
                    "code".to_string(),
                    RouteStatus {
                        available: false,
                        queue_depth: MAX_ROUTE_QUEUE_DEPTH,
                        last_error: Some(
                            "embedding route queue full: depth=10000 max_depth=10000".into(),
                        ),
                        ..RouteStatus::default()
                    },
                ),
                (
                    "notes".to_string(),
                    RouteStatus {
                        available: false,
                        last_error: Some(
                            "VOYAGE_API_KEY or DAYSTROM_VOYAGE_API_KEY is required".into(),
                        ),
                        ..RouteStatus::default()
                    },
                ),
            ]),
        };

        normalize_route_statuses(&mut response);

        assert_eq!(response.routes["code"].health, "unavailable");
        assert_eq!(
            response.routes["code"].health_reason.as_deref(),
            Some("queue_full")
        );
        assert_eq!(response.routes["notes"].health, "unavailable");
        assert_eq!(
            response.routes["notes"].health_reason.as_deref(),
            Some("credential_missing")
        );
    }

    #[test]
    fn token_bucket_counts_requests() {
        let mut bucket = TokenBucket::new(3).unwrap();
        assert!(bucket.try_take_now(2));
        assert!(!bucket.try_take_now(2));
        assert!(bucket.try_take_now(1));
        assert!(!bucket.try_take_now(1));
    }

    #[tokio::test]
    async fn per_project_override_gets_distinct_route_key() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
            [embed.routes]
            code = "voyage"

            [embed.routes.per_project.proj1234]
            code = "ollama"
            "#,
        )
        .unwrap();
        let queue = EmbedQueueHandle::from_router_for_test(
            router,
            Duration::from_millis(10),
            Duration::from_millis(20),
        );
        let mut req = request(Bucket::Code, "a", "h1");
        assert_eq!(queue.resolve_route(&req).unwrap().queue_route, "code");
        req.project_id = Some("proj1234".into());
        assert_eq!(
            queue.resolve_route(&req).unwrap().queue_route,
            "code:proj1234"
        );
        queue.shutdown();
    }

    fn request(bucket: Bucket, entity_id: &str, chunk_hash: &str) -> EmbedRequest {
        request_with_text(bucket, entity_id, chunk_hash, "hello")
    }

    #[test]
    fn document_group_key_strips_only_numeric_chunk_index() {
        assert_eq!(
            document_group_key("project_file:p:f:hash:3"),
            "project_file:p:f:hash"
        );
        assert_eq!(
            document_group_key("knowledge:abcd1234"),
            "knowledge:abcd1234"
        );
        assert_eq!(document_group_key("plain"), "plain");
        assert_eq!(document_group_key("thing:"), "thing:");
    }

    fn ctx_request(doc: &str, idx: usize, text: &str) -> EmbedRequest {
        request_with_text(
            Bucket::Docs,
            &format!("project_file:p:{doc}:hash:{idx}"),
            &format!("h-{doc}-{idx}"),
            text,
        )
    }

    #[test]
    fn grouped_pack_never_splits_a_document_run_across_batches() {
        // Doc A: 100 chunks, doc B: 100 chunks. Cap is 128 docs, so A fits
        // but A+B does not; the batch must close at the A/B boundary.
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        for idx in 0..100 {
            pending.push_back(ctx_request("aaa", idx, "x"));
        }
        for idx in 0..100 {
            pending.push_back(ctx_request("bbb", idx, "x"));
        }
        let first = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(first.len(), 100);
        assert!(
            first.iter().all(|req| req.entity_id.contains(":aaa:")),
            "batch must close at the document boundary"
        );
        let second = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(second.len(), 100);
        assert!(second.iter().all(|req| req.entity_id.contains(":bbb:")));
        assert!(pending.is_empty());
    }

    /// A document whose text exceeds MAX_DOCUMENT_BYTES must window into
    /// sub-documents in SEPARATE batches: two windows of one document in
    /// the same batch would re-merge under consecutive-run grouping and
    /// exceed the contextual model's context window again.
    #[test]
    fn grouped_pack_windows_documents_beyond_context_budget() {
        let big = "x".repeat(30 * 1024);
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        for idx in 0..3 {
            pending.push_back(ctx_request("dense", idx, &big));
        }
        pending.push_back(ctx_request("small", 0, "tiny"));

        // 30KB + 30KB fits the 64KB document budget; the third chunk
        // starts a new window, which must NOT share this batch.
        let first = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(first.len(), 2);
        assert!(first.iter().all(|req| req.entity_id.contains(":dense:")));
        let second = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(second.len(), 2, "remainder window plus the next document");
        assert!(second[0].entity_id.contains(":dense:"));
        assert!(second[1].entity_id.contains(":small:"));
        assert!(pending.is_empty());
    }

    #[test]
    fn grouped_pack_windows_an_oversized_document() {
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        for idx in 0..(MAX_BATCH_DOCS + 40) {
            pending.push_back(ctx_request("huge", idx, "x"));
        }
        let first = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(first.len(), MAX_BATCH_DOCS);
        let second = pack_batch(&mut pending, PackMode::Grouped);
        assert_eq!(second.len(), 40);
        assert!(pending.is_empty());
    }

    #[test]
    fn ungrouped_pack_matches_caps_regardless_of_documents() {
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        for idx in 0..(MAX_BATCH_DOCS + 10) {
            pending.push_back(ctx_request("aaa", idx, "x"));
        }
        let first = pack_batch(&mut pending, PackMode::Flat);
        assert_eq!(first.len(), MAX_BATCH_DOCS);
    }

    fn visual_request(entity_id: &str, byte_len: u64) -> EmbedRequest {
        let mut req = request(Bucket::Docs, entity_id, "h");
        req.visual_kind = Some("image".into());
        req.visual_payload = Some(VisualPayloadRef {
            content_hash: format!("hash-{entity_id}"),
            media_type: "image/png".into(),
            byte_len,
        });
        req
    }

    /// Visual packing weighs by `visual_payload.byte_len`, not the caption
    /// text length: the whole point of `request_weight_bytes`, since a
    /// caption is a few bytes but the image can be tens of megabytes.
    #[test]
    fn visual_pack_caps_by_doc_count_when_payloads_are_small() {
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        for idx in 0..(MAX_VISUAL_BATCH_DOCS + 3) {
            pending.push_back(visual_request(&format!("img-{idx}"), 1024));
        }
        let first = pack_batch(&mut pending, PackMode::Visual);
        assert_eq!(first.len(), MAX_VISUAL_BATCH_DOCS);
        let second = pack_batch(&mut pending, PackMode::Visual);
        assert_eq!(second.len(), 3);
    }

    #[test]
    fn visual_pack_caps_by_byte_budget_when_payloads_are_large() {
        let mut pending: VecDeque<EmbedRequest> = VecDeque::new();
        // Each payload alone is just over half the visual byte budget, so
        // no two can share a batch even though the doc-count cap is not
        // reached.
        let half_plus_one = (MAX_VISUAL_BATCH_BYTES / 2) as u64 + 1;
        for idx in 0..4 {
            pending.push_back(visual_request(&format!("img-{idx}"), half_plus_one));
        }
        let first = pack_batch(&mut pending, PackMode::Visual);
        assert_eq!(first.len(), 1);
        let second = pack_batch(&mut pending, PackMode::Visual);
        assert_eq!(second.len(), 1);
    }

    struct GroupRecordingProvider {
        seen: std::sync::Mutex<Vec<Vec<usize>>>,
    }

    #[async_trait]
    impl EmbeddingProvider for GroupRecordingProvider {
        async fn embed_batch(
            &self,
            inputs: &[EmbedInput],
            _input_type: EmbedInputType,
        ) -> Result<Vec<EmbedOutput>> {
            let shape = inputs
                .iter()
                .map(|input| match input {
                    EmbedInput::DocumentChunks(chunks) => chunks.len(),
                    _ => panic!("contextualized batches must be DocumentChunks"),
                })
                .collect::<Vec<_>>();
            self.seen.lock().unwrap().push(shape.clone());
            Ok(shape
                .into_iter()
                .map(|chunks| EmbedOutput {
                    vectors: (0..chunks).map(|_| vec![0.0_f32; 4]).collect(),
                })
                .collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn document_model(&self) -> &str {
            "mock-context"
        }

        fn endpoint_kind(&self) -> EmbedEndpointKind {
            EmbedEndpointKind::ContextualizedText
        }

        fn id(&self) -> &str {
            "mock-context"
        }
    }

    #[tokio::test]
    async fn contextualized_batches_group_consecutive_chunks_per_document() {
        let provider = GroupRecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let batch = vec![
            ctx_request("aaa", 0, "a0"),
            ctx_request("aaa", 1, "a1"),
            ctx_request("aaa", 2, "a2"),
            ctx_request("bbb", 0, "b0"),
            ctx_request("bbb", 1, "b1"),
        ];
        let vectors = embed_batch_requests(&provider, &batch).await.unwrap();
        assert_eq!(vectors.len(), 5, "one vector per request, in order");
        let seen = provider.seen.lock().unwrap().clone();
        assert_eq!(seen, vec![vec![3, 2]], "two documents, chunks grouped");
    }

    struct VisualRecordingProvider {
        seen: std::sync::Mutex<Vec<Vec<MultimodalPart>>>,
    }

    #[async_trait]
    impl EmbeddingProvider for VisualRecordingProvider {
        async fn embed_batch(
            &self,
            inputs: &[EmbedInput],
            _input_type: EmbedInputType,
        ) -> Result<Vec<EmbedOutput>> {
            let mut seen = self.seen.lock().unwrap();
            for input in inputs {
                match input {
                    EmbedInput::Multimodal(parts) => seen.push(parts.clone()),
                    other => panic!("expected Multimodal input, got {}", other.kind()),
                }
            }
            Ok(inputs
                .iter()
                .map(|_| EmbedOutput::single(vec![0.0_f32; 4]))
                .collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn document_model(&self) -> &str {
            "mock-visual"
        }

        fn endpoint_kind(&self) -> EmbedEndpointKind {
            EmbedEndpointKind::Multimodal
        }

        fn id(&self) -> &str {
            "mock-visual"
        }
    }

    /// `embed_batch_requests` for a multimodal provider must load payload
    /// bytes from the visual sidecar store at request-build time (not
    /// hold bytes on the pending queue earlier) and build interleaved
    /// text+image parts, caption first.
    #[tokio::test]
    async fn visual_requests_load_bytes_and_build_multimodal_parts() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_visual_store::VisualPayloadStore::open(
            dir.path().to_path_buf(),
        ));
        let _guard = bbox_visual_store::install_test_global(store.clone());
        let payload = store.put(b"pretend png bytes", "image/png").unwrap();

        let provider = VisualRecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let mut req = request(Bucket::Docs, "project_file:p:f:h:0", "h1");
        req.text = "figure-3".into();
        req.visual_kind = Some("image".into());
        req.visual_payload = Some(payload.clone());

        let vectors = embed_batch_requests(&provider, &[req]).await.unwrap();
        assert_eq!(vectors.len(), 1);
        let seen = provider.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].len(), 2, "caption text part + image bytes part");
        assert!(matches!(&seen[0][0], MultimodalPart::Text(text) if text == "figure-3"));
        assert!(matches!(
            &seen[0][1],
            MultimodalPart::ImageBytes { mime, bytes }
                if mime == "image/png" && bytes == b"pretend png bytes"
        ));
    }

    /// A thin scan-strip image (aspect ratio far outside the provider's
    /// permitted range) stored in the visual sidecar is padded into range
    /// before it reaches the provider, so it embeds instead of being
    /// poison-dropped (gap-48ae5495). Uses the real production constants
    /// (not a tiny-limits harness) via the same `preflight_part` guard the
    /// provider seam runs, and a fixture small enough (a couple hundred
    /// thousand pixels) to stay fast even after padding — unlike the
    /// pixel-cap class, which needs real multi-megapixel fixtures to trip
    /// under real constants and is covered at unit scale instead
    /// (`visual_normalize::tests`, tiny-limits harness).
    #[tokio::test]
    async fn oversized_aspect_ratio_payload_is_normalized_and_embeds() {
        let dir = tempfile::tempdir().unwrap();
        let store = std::sync::Arc::new(bbox_visual_store::VisualPayloadStore::open(
            dir.path().to_path_buf(),
        ));
        let _guard = bbox_visual_store::install_test_global(store.clone());

        // 100 wide x 2000 tall: a 20:1 ratio, over the real
        // MAX_ASPECT_RATIO (19:1) — the thin OCR scan-strip shape from the
        // gap report, shrunk to keep the fixture fast.
        let thin_strip = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            100,
            2000,
            image::Rgb([40, 40, 40]),
        ));
        let mut buf = std::io::Cursor::new(Vec::new());
        thin_strip
            .write_to(&mut buf, image::ImageFormat::Png)
            .unwrap();
        let raw_bytes = buf.into_inner();

        // Sanity: confirm the fixture actually violates preflight as-is,
        // so the test is exercising the real bug rather than a shape that
        // was never poison to begin with.
        super::super::voyage_multimodal::preflight_part(&MultimodalPart::ImageBytes {
            mime: "image/png".into(),
            bytes: raw_bytes.clone(),
        })
        .expect_err("thin strip fixture should violate the aspect ratio cap before normalization");

        let payload = store.put(&raw_bytes, "image/png").unwrap();
        let provider = VisualRecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let mut req = request(Bucket::Docs, "project_file:p:f:h:0", "h1");
        req.visual_kind = Some("image".into());
        req.visual_payload = Some(payload);

        let vectors = embed_batch_requests(&provider, &[req])
            .await
            .expect("normalized payload should embed instead of being poison-dropped");
        assert_eq!(vectors.len(), 1);

        let seen = provider.seen.lock().unwrap().clone();
        let MultimodalPart::ImageBytes { mime, bytes } = &seen[0][1] else {
            panic!("expected the image part second (caption text is first)");
        };
        assert_ne!(
            bytes, &raw_bytes,
            "provider should have received normalized bytes, not the raw thin-strip payload"
        );
        // The normalized bytes must actually clear preflight now.
        super::super::voyage_multimodal::preflight_part(&MultimodalPart::ImageBytes {
            mime: mime.clone(),
            bytes: bytes.clone(),
        })
        .expect("normalized payload must clear preflight");
    }

    #[tokio::test]
    async fn visual_requests_without_a_payload_ref_are_rejected() {
        let provider = VisualRecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        let mut req = request(Bucket::Docs, "project_file:p:f:h:0", "h1");
        req.visual_kind = Some("image".into());
        let err = embed_batch_requests(&provider, &[req]).await.unwrap_err();
        assert!(err.to_string().contains("no visual payload"));
    }

    /// End-to-end through `EmbedQueueHandle`: a visual enqueue resolves via
    /// the `visual:<kind>` queue route (not a `Bucket` route), packs
    /// through `PackMode::Visual`, and persists a vector: exercising
    /// `resolve_route`'s visual branch and the contradiction-check bucket
    /// guard together.
    #[tokio::test]
    async fn visual_enqueue_routes_through_the_visual_queue_key_and_persists() {
        let dir = tempfile::tempdir().unwrap();
        let payload_store = std::sync::Arc::new(bbox_visual_store::VisualPayloadStore::open(
            dir.path().join("payloads"),
        ));
        let _payload_guard = bbox_visual_store::install_test_global(payload_store.clone());
        let payload = payload_store
            .put(b"pretend png bytes", "image/png")
            .unwrap();

        let vector_store = std::sync::Arc::new(
            bbox_vectors::VectorStore::open(dir.path().join("vectors")).unwrap(),
        );
        let provider = Arc::new(VisualRecordingProvider {
            seen: std::sync::Mutex::new(Vec::new()),
        });
        let queue = EmbedQueueHandle::from_providers(
            vec![(
                "visual:image".to_string(),
                provider.clone() as Arc<dyn EmbeddingProvider>,
                None,
                "visual:image".to_string(),
            )],
            Duration::from_millis(10),
            Duration::from_millis(10),
            None,
            Some(vector_store.clone()),
        );

        let mut req = request(Bucket::Docs, "project_file:p:f:h:0", "h1");
        req.text = "figure-3".into();
        req.visual_kind = Some("image".into());
        req.visual_payload = Some(payload);
        assert!(queue.enqueue(req));
        tokio::time::sleep(Duration::from_millis(80)).await;

        let status = queue.status().routes["visual:image"].clone();
        assert!(status.available, "{status:?}");
        assert_eq!(status.indexed_count, 1);
        assert!(
            vector_store
                .contains_active("visual:image", "project_file:p:f:h:0", "h1")
                .unwrap()
        );
        queue.shutdown();
    }

    /// An unrouted visual kind must surface its error on the `visual:<kind>`
    /// status row, never on the placeholder text bucket the request carries
    /// (`Bucket::Docs`) — marking the bucket made a healthy docs route
    /// report unavailable in `bbox_embed_status`.
    #[tokio::test]
    async fn unrouted_visual_kind_marks_the_visual_route_not_the_bucket() {
        let router = EmbeddingRouter::from_toml_str(
            r#"
            [embed.routes]
            docs = "voyage"
            "#,
        )
        .unwrap();
        let queue = EmbedQueueHandle::from_router_for_test(
            router,
            Duration::from_millis(10),
            Duration::from_millis(20),
        );
        let mut req = request(Bucket::Docs, "project_file:p:f:h:0", "h1");
        req.visual_kind = Some("image".into());
        req.visual_payload = Some(VisualPayloadRef {
            content_hash: "hash".into(),
            media_type: "image/png".into(),
            byte_len: 16,
        });
        assert!(!queue.enqueue(req));

        let routes = queue.status().routes;
        let status = routes
            .get("visual:image")
            .expect("error should be keyed by the visual route label");
        assert!(!status.available);
        assert_eq!(status.health_reason.as_deref(), Some("not_configured"));
        assert!(
            status
                .last_error
                .as_deref()
                .unwrap_or_default()
                .contains("no configured route"),
            "{status:?}"
        );
        assert!(
            !routes.contains_key("docs"),
            "docs bucket must not inherit the visual-lane error: {routes:?}"
        );
        queue.shutdown();
    }

    fn request_with_text(
        bucket: Bucket,
        entity_id: &str,
        chunk_hash: &str,
        text: &str,
    ) -> EmbedRequest {
        EmbedRequest {
            bucket,
            project_id: None,
            entity_id: entity_id.into(),
            chunk_hash: chunk_hash.into(),
            text: text.into(),
            visual_kind: None,
            visual_payload: None,
        }
    }
}

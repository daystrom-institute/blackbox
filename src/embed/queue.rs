use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use parking_lot::RwLock;
use rmcp::schemars;
use serde::Serialize;
use tokio::sync::mpsc;

use super::{Bucket, EmbeddingProvider, EmbeddingRouter};

const DEFAULT_DEBOUNCE: Duration = Duration::from_secs(5);
const DEFAULT_RETRY_BACKOFF: Duration = Duration::from_secs(1);
const MAX_RETRY_BACKOFF: Duration = Duration::from_secs(60);
const MAX_BATCH_RETRIES: u8 = 3;
const MAX_ROUTE_QUEUE_DEPTH: u64 = 10_000;
const MAX_ROUTE_QUEUE_BYTES: u64 = 128 * 1024 * 1024;

type ProviderSpec = (String, Arc<dyn EmbeddingProvider>, Option<u32>, String);

#[derive(Debug, Clone)]
pub struct EmbedRequest {
    pub bucket: Bucket,
    pub project_id: Option<String>,
    pub entity_id: String,
    pub chunk_hash: String,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct RouteStatus {
    pub available: bool,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub dim: Option<usize>,
    pub source_count: Option<u64>,
    pub indexed_count: u64,
    pub session_indexed_count: Option<u64>,
    pub queue_depth: u64,
    pub queue_bytes: u64,
    pub retried_count: u64,
    pub last_error: Option<String>,
    pub coverage_ratio: Option<f32>,
}

impl Default for RouteStatus {
    fn default() -> Self {
        Self {
            available: true,
            provider: None,
            model: None,
            dim: None,
            source_count: None,
            indexed_count: 0,
            session_indexed_count: None,
            queue_depth: 0,
            queue_bytes: 0,
            retried_count: 0,
            last_error: None,
            coverage_ratio: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EmbedStatusResponse {
    pub routes: BTreeMap<String, RouteStatus>,
}

#[derive(Clone)]
pub struct EmbedQueueHandle {
    inner: Arc<EmbedQueueInner>,
}

struct EmbedQueueInner {
    senders: RwLock<BTreeMap<String, mpsc::UnboundedSender<WorkerCommand>>>,
    statuses: Arc<RwLock<BTreeMap<String, RouteStatus>>>,
    router: Option<EmbeddingRouter>,
    vector_store: Option<Arc<crate::vectors::VectorStore>>,
    debounce: Duration,
    retry_backoff: Duration,
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
    vector_store: Option<Arc<crate::vectors::VectorStore>>,
    persist_vectors: bool,
}

impl EmbedQueueHandle {
    pub fn start_default() -> Self {
        Self::start_default_with_store(crate::vectors::global())
    }

    pub fn start_default_with_store(vector_store: Arc<crate::vectors::VectorStore>) -> Self {
        Self::start_default_with_optional_store(Some(vector_store))
    }

    pub fn start_default_without_store() -> Self {
        Self::start_default_with_optional_store(None)
    }

    fn start_default_with_optional_store(
        vector_store: Option<Arc<crate::vectors::VectorStore>>,
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
        vector_store: Option<Arc<crate::vectors::VectorStore>>,
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

    pub fn enqueue(&self, request: EmbedRequest) -> bool {
        let resolved = match self.resolve_route(&request) {
            Ok(resolved) => resolved,
            Err(err) => {
                let fallback = request.bucket.as_str().to_string();
                mark_error(&self.inner.statuses, &fallback, &sanitize_error(&err));
                return false;
            }
        };
        if !self.should_embed(&request, &resolved.vector_route) {
            tracing::debug!(
                route = %resolved.queue_route,
                vector_route = %resolved.vector_route,
                entity_id = %request.entity_id,
                chunk_hash = %request.chunk_hash,
                "embedding enqueue skipped unchanged vector record"
            );
            return false;
        }
        let request_bytes = request.text.len() as u64;
        if !try_reserve_queue(&self.inner.statuses, &resolved.queue_route, request_bytes) {
            tracing::warn!(
                route = %resolved.queue_route,
                entity_id = %request.entity_id,
                request_bytes,
                "embedding enqueue rejected because route queue is full"
            );
            return false;
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
                }
                sent
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
                false
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
        EmbedStatusResponse {
            routes: self.inner.statuses.read().clone(),
        }
    }

    pub fn shutdown(&self) {
        for sender in self.inner.senders.read().values() {
            let _ = sender.send(WorkerCommand::Shutdown);
        }
    }

    fn resolve_route(&self, request: &EmbedRequest) -> Result<ResolvedRoute> {
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
        let provider = match router.route_for(request.bucket, request.project_id.as_deref()) {
            Ok(provider) => provider,
            Err(err) => {
                mark_error(&self.inner.statuses, route, &sanitize_error(&err));
                return None;
            }
        };
        let provider: Arc<dyn EmbeddingProvider> = provider.into();
        let rate_limit_per_min = router.rate_limit_per_min(provider.id());
        let status = RouteStatus {
            provider: Some(provider.id().to_string()),
            model: Some(provider.model_name().to_string()),
            dim: Some(provider.dimensions()),
            ..RouteStatus::default()
        };
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
        tokio::spawn(worker_loop(spec, rx));
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
        vector_store: Option<Arc<crate::vectors::VectorStore>>,
    ) -> Self {
        let statuses = Arc::new(RwLock::new(BTreeMap::new()));
        let mut senders = BTreeMap::new();
        for (route, provider, rate_limit_per_min, vector_route) in providers {
            let status = RouteStatus {
                provider: Some(provider.id().to_string()),
                model: Some(provider.model_name().to_string()),
                dim: Some(provider.dimensions()),
                ..RouteStatus::default()
            };
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
            tokio::spawn(worker_loop(spec, rx));
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
            }),
        }
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
            match collect_quiescent_batch(&mut rx, &mut pending, spec.debounce).await {
                Some(b) => acc.push(b),
                None => return,
            }
            for _ in 1..WORKER_CONCURRENCY {
                if pending.is_empty() {
                    break;
                }
                let mut batch = Vec::new();
                let mut bytes = 0usize;
                while let Some(req) = pending.pop_front() {
                    let req_bytes = req.text.len();
                    if !batch.is_empty()
                        && (batch.len() >= MAX_BATCH_DOCS || bytes + req_bytes > MAX_BATCH_BYTES)
                    {
                        pending.push_front(req);
                        break;
                    }
                    bytes += req_bytes;
                    batch.push(req);
                    if batch.len() >= MAX_BATCH_DOCS {
                        break;
                    }
                }
                if !batch.is_empty() {
                    acc.push(batch);
                }
            }
            acc
        };
        if let Some(limiter) = &mut rate_limiter {
            for batch in batches {
                limiter.acquire(1).await;
                let texts = batch.iter().map(|req| req.text.clone()).collect::<Vec<_>>();
                let result = spec.provider.embed_batch(&texts).await;
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
                .map(|batch| {
                    let texts = batch.iter().map(|req| req.text.clone()).collect::<Vec<_>>();
                    async move { provider.embed_batch(&texts).await }
                })
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
                model = spec.provider.model_name(),
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
const MAX_BATCH_DOCS: usize = 128;
const MAX_BATCH_BYTES: usize = 100 * 1024;
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
    let mut batch = Vec::new();
    let mut bytes = 0usize;
    while let Some(req) = pending.pop_front() {
        let req_bytes = req.text.len();
        if !batch.is_empty()
            && (batch.len() >= MAX_BATCH_DOCS || bytes + req_bytes > MAX_BATCH_BYTES)
        {
            pending.push_front(req);
            break;
        }
        bytes += req_bytes;
        batch.push(req);
        if batch.len() >= MAX_BATCH_DOCS {
            break;
        }
    }
    Some(batch)
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
            if request.bucket == Bucket::Knowledge {
                contradiction_checks.push((request.clone(), vector.clone()));
            }
            crate::vectors::VectorUpsert {
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
        crate::embed_queue::maybe_detect_knowledge_contradiction(
            &request,
            &spec.vector_route,
            &vector,
        );
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
) -> bool {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    if status.queue_depth >= MAX_ROUTE_QUEUE_DEPTH
        || status.queue_bytes.saturating_add(bytes) > MAX_ROUTE_QUEUE_BYTES
    {
        status.available = false;
        status.last_error = Some(format!(
            "embedding route queue full: depth={} bytes={} max_depth={} max_bytes={}",
            status.queue_depth, status.queue_bytes, MAX_ROUTE_QUEUE_DEPTH, MAX_ROUTE_QUEUE_BYTES
        ));
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
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.available = true;
    status.indexed_count = status.indexed_count.saturating_add(count);
    status.queue_depth = status.queue_depth.saturating_sub(count);
    status.queue_bytes = status.queue_bytes.saturating_sub(bytes);
    status.last_error = None;
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
    status.last_error = Some(message.to_string());
}

fn mark_error(statuses: &RwLock<BTreeMap<String, RouteStatus>>, route: &str, message: &str) {
    let mut statuses = statuses.write();
    let status = statuses.entry(route.to_string()).or_default();
    status.available = false;
    status.last_error = Some(message.to_string());
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
    async fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Err(anyhow!(self.error.clone()))
    }

    fn dimensions(&self) -> usize {
        0
    }

    fn model_name(&self) -> &str {
        "unavailable"
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
        async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                return Err(anyhow!("provider unavailable: token redacted"));
            }
            Ok(texts.iter().map(|_| vec![0.0_f32; 4]).collect())
        }

        fn dimensions(&self) -> usize {
            4
        }

        fn model_name(&self) -> &str {
            "mock"
        }

        fn id(&self) -> &str {
            "mock"
        }
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
        EmbedRequest {
            bucket,
            project_id: None,
            entity_id: entity_id.into(),
            chunk_hash: chunk_hash.into(),
            text: "hello".into(),
        }
    }
}

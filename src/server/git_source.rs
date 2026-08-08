//! Authenticated typed Git-history producer intake.
//!
//! The HTTP lane owns bounded parsing and streaming. Catalog authority stays
//! in `producer_auth`; durable resumability and graph verification stay in
//! `bbox-git-source-store`.

use std::io::SeekFrom;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Weak};

use anyhow::{Result, bail};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bbox_code_source::ErrorResponse;
use bbox_git_source::{
    BeginGitHistoryUploadRequestV1, ContractError, GitHistoryManifestPageV1,
    GitHistoryProbeRequestV1, GitHistoryProbeResponseV1, GitHistorySourceStatusV1, GitSourceLimits,
    MAX_HISTORY_MANIFEST_PAGE_BYTES, MAX_HISTORY_RECORD_BYTES, MissingHistoryRecordsPageV1,
    ProvenanceExportPageResponseV1, ProvenanceExportPullRequestV1, ProvenanceExportReceiptV1,
    SCHEMA_VERSION,
};
use bbox_git_source_store::{
    GitSourceStore, HistoryTransportAuthorityV1, StoreLimits, StoreRequestError,
};
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::SharedState;
use super::producer_auth::{ProducerGrant, RepoTransportGrant, RepoTransportGrantError};

const UPLOAD_BODY_TEMP_PREFIX: &str = ".git-source-upload-body-";
const UPLOAD_BODY_TEMP_SUFFIX: &str = ".tmp";
const MAX_PROVENANCE_OBSERVED_SCAN_BYTES: u64 = 512 * 1024 * 1024;
const MAX_PROVENANCE_SELECTED_EDGE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CACHED_PROVENANCE_PLAN_BYTES: u64 = 64 * 1024 * 1024;
const MAX_CACHED_PROVENANCE_EXPORTS: usize = 4;

struct CachedProvenanceExport {
    observed_version_token: String,
    observed_content_sha256: String,
    relation_index: Weak<bbox_edge_index::edge_index::EdgeIndex>,
    plan: Arc<bbox_provenance::ProvenanceExportPlan>,
    logical_bytes: u64,
    ordered_document_commitment: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct ProvenanceExportMetricsV1 {
    pub pages_served: u64,
    pub stale_restarts: u64,
    pub receipts_accepted: u64,
}

pub(crate) struct GitSourceRuntime {
    store: Arc<GitSourceStore>,
    activation_tx: std::sync::mpsc::SyncSender<String>,
    activation_rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<String>>>,
    /// Repo id -> `(activation-journal checksum, Tantivy searcher generation)`
    /// proven against the exact commit lane and durable snapshot receipts.
    /// Redrive reuses the proof only while both authorities are unchanged;
    /// any index commit forces a fresh exact-view probe.
    validated_activations: parking_lot::Mutex<std::collections::BTreeMap<String, (String, u64)>>,
    provenance_exports: parking_lot::Mutex<
        std::collections::BTreeMap<(String, String), Arc<CachedProvenanceExport>>,
    >,
    provenance_pages_served: AtomicU64,
    provenance_stale_restarts: AtomicU64,
    provenance_receipts_accepted: AtomicU64,
}

impl GitSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config) -> Result<Self> {
        let store = Arc::new(GitSourceStore::open(
            config.paths.state_dir.join("git-sources"),
            checked_store_limits(config)?,
        )?);
        reap_upload_body_tempfiles(store.root())?;
        let (activation_tx, activation_rx) = std::sync::mpsc::sync_channel(64);
        Ok(Self {
            store,
            activation_tx,
            activation_rx: std::sync::Mutex::new(Some(activation_rx)),
            validated_activations: parking_lot::Mutex::new(Default::default()),
            provenance_exports: parking_lot::Mutex::new(Default::default()),
            provenance_pages_served: AtomicU64::new(0),
            provenance_stale_restarts: AtomicU64::new(0),
            provenance_receipts_accepted: AtomicU64::new(0),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        let (activation_tx, activation_rx) = std::sync::mpsc::sync_channel(64);
        Self {
            store: Arc::new(
                GitSourceStore::open(root.join("git-sources"), StoreLimits::default()).unwrap(),
            ),
            activation_tx,
            activation_rx: std::sync::Mutex::new(Some(activation_rx)),
            validated_activations: parking_lot::Mutex::new(Default::default()),
            provenance_exports: parking_lot::Mutex::new(Default::default()),
            provenance_pages_served: AtomicU64::new(0),
            provenance_stale_restarts: AtomicU64::new(0),
            provenance_receipts_accepted: AtomicU64::new(0),
        }
    }

    pub(crate) fn store(&self) -> Arc<GitSourceStore> {
        self.store.clone()
    }

    pub(crate) fn update_limits(&self, config: &crate::config::Config) -> Result<()> {
        self.store.update_limits(checked_store_limits(config)?)
    }

    pub(crate) fn enqueue_activation(&self, source_generation_id: String) {
        match self.activation_tx.try_send(source_generation_id) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(_)) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(_)) => {
                tracing::warn!("Git-history activation worker is unavailable")
            }
        }
    }

    pub(crate) fn take_activation_receiver(&self) -> Option<std::sync::mpsc::Receiver<String>> {
        self.activation_rx.lock().ok()?.take()
    }

    pub(crate) fn activation_was_validated(
        &self,
        repo_history_id: &str,
        journal_checksum: &str,
        searcher_generation: u64,
    ) -> bool {
        self.validated_activations
            .lock()
            .get(repo_history_id)
            .is_some_and(|current| {
                current.0 == journal_checksum && current.1 == searcher_generation
            })
    }

    pub(crate) fn mark_activation_validated(
        &self,
        repo_history_id: &str,
        journal_checksum: &str,
        searcher_generation: u64,
    ) {
        self.validated_activations.lock().insert(
            repo_history_id.to_string(),
            (journal_checksum.to_string(), searcher_generation),
        );
    }

    pub(crate) fn validate_config(config: &crate::config::Config) -> Result<()> {
        checked_store_limits(config).map(|_| ())
    }

    pub(crate) fn provenance_export_metrics(&self) -> ProvenanceExportMetricsV1 {
        ProvenanceExportMetricsV1 {
            pages_served: self.provenance_pages_served.load(Ordering::Relaxed),
            stale_restarts: self.provenance_stale_restarts.load(Ordering::Relaxed),
            receipts_accepted: self.provenance_receipts_accepted.load(Ordering::Relaxed),
        }
    }

    fn cache_provenance_export(
        &self,
        cache_key: (String, String),
        project_id: &str,
        export: Arc<CachedProvenanceExport>,
    ) {
        let mut cache = self.provenance_exports.lock();
        // Producer credential rotation must not retain another full plan for
        // the same catalog project.
        cache.retain(|(_, cached_project), _| cached_project != project_id);
        while cache.len() >= MAX_CACHED_PROVENANCE_EXPORTS {
            let Some(eviction) = cache.keys().next().cloned() else {
                break;
            };
            cache.remove(&eviction);
        }
        cache.insert(cache_key, export);
    }
}

fn store_limits(config: &crate::config::Config) -> StoreLimits {
    StoreLimits {
        contract: GitSourceLimits {
            max_history_commits: config.code_collection.max_git_history_commits,
            max_history_logical_bytes: config.code_collection.max_git_history_logical_bytes,
            max_provenance_documents: config.code_collection.max_provenance_documents,
            max_provenance_logical_bytes: config.code_collection.max_provenance_logical_bytes,
        },
        max_open_uploads_per_producer: config.code_collection.max_open_uploads_per_producer,
        retained_history_generations: config.code_collection.retained_generations,
        unreferenced_record_grace_secs: config
            .code_collection
            .unreferenced_blob_grace_hours
            .saturating_mul(60 * 60),
    }
}

fn checked_store_limits(config: &crate::config::Config) -> Result<StoreLimits> {
    let limits = store_limits(config);
    if limits.max_open_uploads_per_producer == 0
        || limits.retained_history_generations == 0
        || limits.contract.max_history_commits == 0
        || limits.contract.max_history_logical_bytes == 0
        || limits.contract.max_provenance_documents == 0
        || limits.contract.max_provenance_logical_bytes == 0
    {
        bail!("Git-source limits must be nonzero");
    }
    Ok(limits)
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/code-source/v1/git-history/probe",
            post(probe_history).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads",
            post(begin_history_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads/{upload_id}/manifest/{page}",
            put(put_history_manifest_page)
                .layer(DefaultBodyLimit::max(MAX_HISTORY_MANIFEST_PAGE_BYTES)),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads/{upload_id}/manifest/complete",
            post(complete_history_manifest).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads/{upload_id}/missing",
            get(missing_history_records),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads/{upload_id}/records/{hash}",
            put(put_history_record).layer(DefaultBodyLimit::max(MAX_HISTORY_RECORD_BYTES as usize)),
        )
        .route(
            "/internal/code-source/v1/git-history/uploads/{upload_id}/finalize",
            post(finalize_history_upload).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/git-history/generations/{generation}/status",
            get(history_generation_status),
        )
        .route(
            "/internal/code-source/v1/provenance/export/page",
            post(provenance_export_page).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/code-source/v1/provenance/export/receipt",
            post(provenance_export_receipt).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_git_source_request,
        ))
}

async fn provenance_export_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<ProvenanceExportPullRequestV1>,
) -> Result<Json<ProvenanceExportPageResponseV1>, HttpError> {
    request
        .scope
        .validate()
        .map_err(|_| HttpError::unprocessable("invalid_scope", "published scope is invalid"))?;
    if request.cursor.is_some() && request.generation.is_none() {
        return Err(stale_generation(
            "generation is required with a provenance cursor",
        ));
    }
    let project_id = require_project_grant(&state, &grant, &request.scope)?;
    let cache_key = (grant.producer_id.clone(), project_id.clone());
    let edges_dir = provenance_edges_dir(&state);
    let current =
        bbox_edge_sidecar::edge_sidecar::observed_edge_lane_version(&edges_dir, &project_id)
            .map_err(HttpError::storage)?;
    let relation_index = state.code_read_view.read().edge_index.clone();
    let notes_ref = bbox_corpus_core::git::notes_ref("provenance").map_err(HttpError::storage)?;
    let limits = state
        .git_sources
        .store()
        .current_contract_limits()
        .map_err(HttpError::storage)?;

    let cached = state
        .git_sources
        .provenance_exports
        .lock()
        .get(&cache_key)
        .filter(|cached| {
            cached.observed_version_token == current.version_token
                && cached.plan.scope == request.scope
                && cached.plan.notes_ref == notes_ref
                && cached
                    .relation_index
                    .upgrade()
                    .is_some_and(|cached_index| Arc::ptr_eq(&cached_index, &relation_index))
                && cached.plan.document_count() <= limits.max_provenance_documents
                && cached.logical_bytes <= limits.max_provenance_logical_bytes
        })
        .cloned();
    let cached = match (request.generation.as_deref(), cached) {
        (Some(expected), Some(cached)) if cached.plan.generation == expected => cached,
        (Some(_), _) => {
            state
                .git_sources
                .provenance_stale_restarts
                .fetch_add(1, Ordering::Relaxed);
            return Err(stale_generation("provenance inventory changed"));
        }
        (None, Some(cached)) => cached,
        (None, None) => {
            let scope = request.scope.clone();
            let built = build_provenance_export(
                edges_dir,
                scope,
                project_id.clone(),
                notes_ref,
                relation_index,
                limits,
            )
            .await
            .map_err(|error| {
                if error.body.code == "provenance_export_stale_generation" {
                    state
                        .git_sources
                        .provenance_stale_restarts
                        .fetch_add(1, Ordering::Relaxed);
                }
                error
            })?;
            state
                .git_sources
                .cache_provenance_export(cache_key, &project_id, built.clone());
            built
        }
    };
    let params = bbox_mcp_tools::mcp_tools::provenance_plan::ProvenanceExportPlanParams {
        cursor: request.cursor,
        generation: request.generation,
    };
    let page = bbox_mcp_tools::mcp_tools::provenance_plan::export_plan_page_from_plan(
        &params,
        &cached.plan,
    )
    .map_err(HttpError::from_provenance_plan)?;
    state
        .git_sources
        .provenance_pages_served
        .fetch_add(1, Ordering::Relaxed);
    Ok(Json(ProvenanceExportPageResponseV1 {
        schema_version: SCHEMA_VERSION,
        document_count: cached.plan.document_count(),
        logical_bytes: cached.logical_bytes,
        ordered_document_commitment: cached.ordered_document_commitment.clone(),
        page,
    }))
}

async fn provenance_export_receipt(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(receipt): Json<ProvenanceExportReceiptV1>,
) -> Result<StatusCode, HttpError> {
    let limits = state
        .git_sources
        .store()
        .current_contract_limits()
        .map_err(HttpError::storage)?;
    receipt
        .validate(limits)
        .map_err(|error| HttpError::from_contract(&error))?;
    let project_id = require_project_grant(&state, &grant, &receipt.scope)?;
    let cache_key = (grant.producer_id.clone(), project_id.clone());
    let cached = state
        .git_sources
        .provenance_exports
        .lock()
        .get(&cache_key)
        .cloned();
    let Some(cached) = cached else {
        state
            .git_sources
            .provenance_stale_restarts
            .fetch_add(1, Ordering::Relaxed);
        return Err(stale_generation(
            "provenance export plan is no longer resident",
        ));
    };
    let edges_dir = provenance_edges_dir(&state);
    let current =
        bbox_edge_sidecar::edge_sidecar::observed_edge_lane_version(&edges_dir, &project_id)
            .map_err(HttpError::storage)?;
    let relation_index = state.code_read_view.read().edge_index.clone();
    let notes_ref = bbox_corpus_core::git::notes_ref("provenance").map_err(HttpError::storage)?;
    if current.version_token != cached.observed_version_token
        || !cached
            .relation_index
            .upgrade()
            .is_some_and(|cached_index| Arc::ptr_eq(&cached_index, &relation_index))
        || receipt.generation != cached.plan.generation
        || receipt.notes_ref != notes_ref
        || cached.logical_bytes > limits.max_provenance_logical_bytes
    {
        state
            .git_sources
            .provenance_stale_restarts
            .fetch_add(1, Ordering::Relaxed);
        return Err(stale_generation(
            "provenance inventory changed before receipt",
        ));
    }
    if receipt.scope != cached.plan.scope
        || receipt.notes_ref != cached.plan.notes_ref
        || receipt.document_count != cached.plan.document_count()
        || receipt.ordered_document_commitment != cached.ordered_document_commitment
    {
        return Err(HttpError::unprocessable(
            "provenance_export_receipt_mismatch",
            "provenance receipt does not match the exported plan",
        ));
    }
    let store = state.git_sources.store();
    let producer_id = grant.producer_id;
    let stored_project_id = project_id.clone();
    let observed_commitment = cached.observed_content_sha256.clone();
    blocking(move || {
        store.record_provenance_export_receipt(&producer_id, &stored_project_id, receipt)
    })
    .await?;
    state
        .git_sources
        .provenance_receipts_accepted
        .fetch_add(1, Ordering::Relaxed);
    tracing::info!(
        project_id,
        observed_lane_sha256 = observed_commitment,
        "accepted provenance export receipt"
    );
    Ok(StatusCode::NO_CONTENT)
}

async fn build_provenance_export(
    edges_dir: std::path::PathBuf,
    scope: bbox_corpus_core::identity::PublishedScope,
    project_id: String,
    notes_ref: String,
    relation_index: Arc<bbox_edge_index::edge_index::EdgeIndex>,
    limits: GitSourceLimits,
) -> Result<Arc<CachedProvenanceExport>, HttpError> {
    tokio::task::spawn_blocking(move || {
        let mut selected = Vec::new();
        let mut selected_bytes = 0_u64;
        let snapshot = bbox_edge_sidecar::edge_sidecar::visit_observed_edge_lane(
            &edges_dir,
            &project_id,
            // This is both a locality guard and an operational safety bound:
            // export never needs to scan a source larger than the configured
            // provenance lane budget.
            limits
                .max_provenance_logical_bytes
                .min(MAX_PROVENANCE_OBSERVED_SCAN_BYTES),
            bbox_git_source::MAX_PROVENANCE_DOCUMENT_BYTES as usize,
            |edge| {
                if !matches!(edge.kind.as_str(), "EDITED_FILE" | "READ_FILE")
                    || edge.metadata.get("anchor.project_id").map(String::as_str)
                        != Some(project_id.as_str())
                {
                    return Ok(());
                }
                selected_bytes = selected_bytes
                    .checked_add(serde_json::to_vec(&edge)?.len() as u64)
                    .ok_or_else(|| anyhow::anyhow!("provenance edge inventory size overflow"))?;
                if selected_bytes > MAX_PROVENANCE_SELECTED_EDGE_BYTES {
                    anyhow::bail!("selected provenance edges exceed the export memory limit");
                }
                selected.push(edge);
                Ok(())
            },
        )?;
        let plan = bbox_mcp_tools::mcp_tools::provenance_plan::build_plan_from_observed_edges(
            scope,
            &project_id,
            &notes_ref,
            selected.iter(),
            &relation_index,
        )?;
        let logical_bytes = plan
            .documents
            .iter()
            .try_fold(0_u64, |total, document| {
                total.checked_add(document.document.len() as u64)
            })
            .ok_or_else(|| anyhow::anyhow!("provenance plan logical size overflow"))?;
        if plan.document_count() > limits.max_provenance_documents
            || logical_bytes > limits.max_provenance_logical_bytes
            || logical_bytes > MAX_CACHED_PROVENANCE_PLAN_BYTES
        {
            anyhow::bail!("provenance export plan exceeds an enforced limit");
        }
        let ordered_document_commitment = plan.ordered_document_commitment()?;
        Ok::<_, anyhow::Error>(Arc::new(CachedProvenanceExport {
            observed_version_token: snapshot.version_token,
            observed_content_sha256: snapshot.content_sha256,
            relation_index: Arc::downgrade(&relation_index),
            plan: Arc::new(plan),
            logical_bytes,
            ordered_document_commitment,
        }))
    })
    .await
    .map_err(|_| HttpError::storage("provenance export planner task failed"))?
    .map_err(HttpError::from_provenance_plan)
}

async fn probe_history(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<GitHistoryProbeRequestV1>,
) -> Result<Json<GitHistoryProbeResponseV1>, HttpError> {
    request
        .validate()
        .map_err(|error| HttpError::from_contract(&error))?;
    let repo_grant = require_repo_grant(&state, &grant, &request.scope)?;
    let store = state.git_sources.store();
    let producer_id = grant.producer_id;
    let repo_history_id = repo_grant.repo_history_id;
    let repo_head = request.repo_head;
    let object_format = request.object_format;
    let current = blocking(move || {
        store.probe_ready_history(&producer_id, &repo_history_id, &repo_head, object_format)
    })
    .await?
    .map(|source| status_from_source(&source));
    Ok(Json(GitHistoryProbeResponseV1 { current }))
}

async fn begin_history_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(mut request): Json<BeginGitHistoryUploadRequestV1>,
) -> Result<impl IntoResponse, HttpError> {
    let repo_grant = require_repo_grant(&state, &grant, &request.descriptor.scope)?;
    // The caller supplies only a member scope for authorization. Persist the
    // catalog-derived canonical repo scope so one monorepo has one source.
    request.descriptor.scope = repo_grant.authority_scope;
    let store = state.git_sources.store();
    let producer_id = grant.producer_id;
    let response = blocking(move || {
        store.begin_history_upload(
            &producer_id,
            &repo_grant.repo_history_id,
            &repo_grant.primary_namespace,
            request.descriptor,
        )
    })
    .await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn put_history_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, page)): Path<(String, u32)>,
    Json(page_body): Json<GitHistoryManifestPageV1>,
) -> Result<StatusCode, HttpError> {
    let store = state.git_sources.store();
    require_upload_grant(&state, &store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id;
    blocking(move || store.put_history_manifest_page(&producer_id, &upload_id, page, &page_body))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn complete_history_manifest(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<Json<MissingHistoryRecordsPageV1>, HttpError> {
    let store = state.git_sources.store();
    require_upload_grant(&state, &store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id;
    Ok(Json(
        blocking(move || store.complete_history_manifest(&producer_id, &upload_id)).await?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingQuery {
    cursor: Option<String>,
}

async fn missing_history_records(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingHistoryRecordsPageV1>, HttpError> {
    let store = state.git_sources.store();
    require_upload_grant(&state, &store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id;
    Ok(Json(
        blocking(move || {
            store.missing_history_records(&producer_id, &upload_id, query.cursor.as_deref())
        })
        .await?,
    ))
}

async fn put_history_record(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let store = state.git_sources.store();
    require_upload_grant(&state, &store, &grant, &upload_id).await?;
    let expected_size = {
        let store = store.clone();
        let producer_id = grant.producer_id.clone();
        let upload_id = upload_id.clone();
        let hash = hash.clone();
        blocking(move || store.expected_history_record_size(&producer_id, &upload_id, &hash))
            .await?
    };
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            HttpError::unprocessable("content_length_required", "exact Content-Length required")
        })?;
    if content_length != expected_size {
        return Err(HttpError::unprocessable(
            "history_record_size_mismatch",
            "Content-Length does not match the manifest",
        ));
    }

    let temporary = tempfile::Builder::new()
        .prefix(UPLOAD_BODY_TEMP_PREFIX)
        .suffix(UPLOAD_BODY_TEMP_SUFFIX)
        .tempfile_in(store.root())
        .map_err(HttpError::storage)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen().map_err(HttpError::storage)?);
    let mut stream = body.into_data_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| HttpError::unprocessable("invalid_body", error.to_string()))?;
        written = written.checked_add(chunk.len() as u64).ok_or_else(|| {
            HttpError::too_large("history_record_too_large", "history record is too large")
        })?;
        if written > expected_size {
            return Err(HttpError::too_large(
                "history_record_too_large",
                "history record exceeds its manifest size",
            ));
        }
        file.write_all(&chunk).await.map_err(HttpError::storage)?;
    }
    if written != expected_size {
        return Err(HttpError::unprocessable(
            "history_record_size_mismatch",
            "history record is shorter than its manifest size",
        ));
    }
    file.sync_all().await.map_err(HttpError::storage)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(HttpError::storage)?;
    let file = file.into_std().await;
    let producer_id = grant.producer_id;
    blocking(move || {
        store.install_history_record(&producer_id, &upload_id, &hash, expected_size, file)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize_history_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<impl IntoResponse, HttpError> {
    let store = state.git_sources.store();
    require_upload_grant(&state, &store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id;
    let response =
        blocking(move || store.finalize_history_upload(&producer_id, &upload_id)).await?;
    state
        .git_sources
        .enqueue_activation(response.source_generation_id.clone());
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn history_generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(generation): Path<String>,
) -> Result<Json<GitHistorySourceStatusV1>, HttpError> {
    let store = state.git_sources.store();
    require_generation_grant(&state, &store, &grant, &generation).await?;
    let producer_id = grant.producer_id;
    Ok(Json(
        blocking(move || store.history_status(&producer_id, &generation)).await?,
    ))
}

fn require_repo_grant(
    state: &SharedState,
    grant: &ProducerGrant,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> Result<RepoTransportGrant, HttpError> {
    let auth = state.code_sources.producer_auth();
    if !auth.git_transport_enabled() {
        return Err(HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "git_transport_disabled",
            "Git transport is disabled",
        ));
    }
    auth.repo_transport_grant(grant, scope)
        .cloned()
        .map_err(HttpError::from_grant)
}

fn require_project_grant(
    state: &SharedState,
    grant: &ProducerGrant,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> Result<String, HttpError> {
    let auth = state.code_sources.producer_auth();
    if !auth.git_transport_enabled() {
        return Err(HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "provenance_transport_disabled",
            "provenance transport is disabled",
        ));
    }
    auth.project_transport_grant(grant, scope)
        .map(|project_id| project_id.as_str().to_string())
        .map_err(HttpError::from_grant)
}

fn stale_generation(message: &str) -> HttpError {
    HttpError::new(
        StatusCode::CONFLICT,
        "provenance_export_stale_generation",
        message,
    )
}

fn provenance_edges_dir(state: &SharedState) -> std::path::PathBuf {
    bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(
        &state.idx.read().reindex_config().projects_path,
    )
}

async fn require_upload_grant(
    state: &SharedState,
    store: &Arc<GitSourceStore>,
    grant: &ProducerGrant,
    upload_id: &str,
) -> Result<RepoTransportGrant, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let upload_id = upload_id.to_string();
    let authority = blocking(move || store.upload_authority(&producer_id, &upload_id)).await?;
    require_matching_authority(state, grant, authority)
}

async fn require_generation_grant(
    state: &SharedState,
    store: &Arc<GitSourceStore>,
    grant: &ProducerGrant,
    generation: &str,
) -> Result<RepoTransportGrant, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let generation = generation.to_string();
    let authority = blocking(move || store.generation_authority(&producer_id, &generation)).await?;
    require_matching_authority(state, grant, authority)
}

fn require_matching_authority(
    state: &SharedState,
    grant: &ProducerGrant,
    authority: HistoryTransportAuthorityV1,
) -> Result<RepoTransportGrant, HttpError> {
    let current = require_repo_grant(state, grant, &authority.scope)?;
    if current.repo_history_id != authority.repo_history_id
        || current.primary_namespace != authority.primary_namespace
    {
        return Err(HttpError::new(
            StatusCode::CONFLICT,
            "repo_authority_changed",
            "repository transport authority changed",
        ));
    }
    Ok(current)
}

fn status_from_source(
    source: &bbox_git_source_store::StoredHistorySourceV1,
) -> GitHistorySourceStatusV1 {
    GitHistorySourceStatusV1 {
        source_generation_id: source.source_generation_id.clone(),
        state: source.state,
        commit_count: source.descriptor.commit_count,
        logical_bytes: source.descriptor.logical_bytes,
        diagnostic: source.diagnostic.clone(),
    }
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::storage("Git-source blocking task failed"))?
        .map_err(HttpError::from_store)
}

fn reap_upload_body_tempfiles(store_root: &std::path::Path) -> Result<u64> {
    let mut reaped = 0_u64;
    for entry in std::fs::read_dir(store_root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !name.starts_with(UPLOAD_BODY_TEMP_PREFIX) || !name.ends_with(UPLOAD_BODY_TEMP_SUFFIX) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        std::fs::remove_file(entry.path())?;
        reaped = reaped.saturating_add(1);
    }
    if reaped > 0 {
        std::fs::File::open(store_root)?.sync_all()?;
    }
    Ok(reaped)
}

#[derive(Debug)]
struct HttpError {
    status: StatusCode,
    body: ErrorResponse,
}

impl HttpError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorResponse {
                code: code.to_string(),
                message: message.into().chars().take(512).collect(),
            },
        }
    }

    fn unprocessable(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::UNPROCESSABLE_ENTITY, code, message)
    }

    fn too_large(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::PAYLOAD_TOO_LARGE, code, message)
    }

    fn from_grant(error: RepoTransportGrantError) -> Self {
        let status = match error {
            RepoTransportGrantError::ScopeForbidden => StatusCode::FORBIDDEN,
            RepoTransportGrantError::RepoHistoryNotFound
            | RepoTransportGrantError::RepoHistoryScopeSplit => StatusCode::CONFLICT,
        };
        Self::new(
            status,
            error.code(),
            "repository transport authority unavailable",
        )
    }

    fn from_contract(error: &ContractError) -> Self {
        match error {
            ContractError::HistoryLimitExceeded
            | ContractError::HistoryRecordTooLarge
            | ContractError::ProvenanceLimitExceeded => Self::too_large(
                "limit_exceeded",
                "Git-source input exceeds an enforced limit",
            ),
            ContractError::UnsupportedSchema(_) => Self::unprocessable(
                "unsupported_contract",
                "Git-source contract version is unsupported",
            ),
            _ => Self::unprocessable(
                "invalid_git_source_input",
                "Git-source input violates the transport contract",
            ),
        }
    }

    fn from_provenance_plan(error: anyhow::Error) -> Self {
        let message = error.to_string();
        if message.contains("stale_generation") || message.contains("changed while") {
            return stale_generation("provenance inventory changed");
        }
        if message.contains("exceed")
            || message.contains("too_large")
            || message.contains("byte_limit")
            || message.contains("line_limit")
        {
            return Self::too_large(
                "limit_exceeded",
                "provenance export exceeds an enforced limit",
            );
        }
        tracing::warn!(error = %error, "provenance export plan failed");
        Self::unprocessable(
            "provenance_document_invalid",
            "observed provenance source cannot form a valid export",
        )
    }

    fn storage(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "Git-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "Git-source storage is unavailable",
        )
    }

    fn from_store(error: anyhow::Error) -> Self {
        if error
            .chain()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        {
            return Self::storage(error);
        }
        if let Some(contract) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ContractError>())
        {
            return Self::from_contract(contract);
        }
        if let Some(request) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreRequestError>())
        {
            return match request {
                StoreRequestError::LimitExceeded => Self::too_large(
                    "limit_exceeded",
                    "Git-source input exceeds an enforced limit",
                ),
                StoreRequestError::TooManyOpenUploads => Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "upload_limit_reached",
                    "producer has too many open Git-source uploads",
                ),
                StoreRequestError::InvalidState => Self::unprocessable(
                    "invalid_upload_state",
                    "Git-source upload is not in the required state",
                ),
                StoreRequestError::InvalidInput => {
                    Self::unprocessable("invalid_git_source_input", "Git-source input is invalid")
                }
                StoreRequestError::NotFound => {
                    Self::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
                }
            };
        }
        Self::storage(error)
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::io::Write as _;

    use axum::body::to_bytes;
    use axum::http::Request;
    use bbox_chunker::{EdgeConfidence, EdgeProvenance};
    use bbox_corpus_core::entity_ref::EntityRef;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CommitNamespace, CorpusProject, ProjectId, ProjectScope,
        RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
        RepoHistoryRecord,
    };
    use bbox_git_source::{
        BeginGitHistoryUploadResponseV1, GitHistoryCommitFragmentV1, GitHistoryCommitHeaderV1,
        GitHistoryDescriptorV1, GitHistoryManifestEntryV1, GitObjectFormatV1, SCHEMA_VERSION,
        encode_history_fragment, history_manifest_sha256,
    };
    use sha2::{Digest, Sha256};
    use tower::ServiceExt;

    use super::*;
    use crate::server::producer_auth::ProducerAuthRuntime;

    fn enabled_state(root: &std::path::Path) -> (Arc<SharedState>, String, PublishedScope) {
        let state = Arc::new(SharedState::for_test(root));
        let token_secret = "9".repeat(64);
        let token = bro_rpc::ServiceToken::parse(token_secret.clone()).unwrap();
        let scope = PublishedScope::try_new("git-http-repo", ".").unwrap();
        let project_id = ProjectId::parse("p_00000000000000000000000000000001").unwrap();
        let repo_history_id = RepoHistoryId::parse("rh_00000000000000000000000000000001").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id: repo_history_id.clone(),
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("git-http-repo").unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("git-http-repo").unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        );
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Git HTTP fixture".into(),
                created_at: "2026-08-08T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: Some(repo_history_id),
                languages: BTreeSet::new(),
            },
        );
        catalog.validate().unwrap();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(
                    token,
                    ProducerGrant {
                        producer_id: "git-http-producer".into(),
                        projects: BTreeMap::from([(
                            scope.clone(),
                            project_id.as_str().to_string(),
                        )]),
                    },
                )],
                &catalog,
            )));
        (state, token_secret, scope)
    }

    fn request(method: &str, uri: &str, token: Option<&str>, body: Body) -> Request<Body> {
        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        builder.body(body).unwrap()
    }

    #[tokio::test]
    async fn authenticated_history_routes_reach_durable_ready_state() {
        let directory = tempfile::tempdir().unwrap();
        let (state, token, scope) = enabled_state(directory.path());
        let app = router(state.clone()).with_state(state.clone());

        // Authentication is outside bounded JSON parsing: malformed bytes
        // without a credential still stop at the authorization boundary.
        let denied = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/git-history/uploads",
                None,
                Body::from("{"),
            ))
            .await
            .unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        let commit = "1".repeat(40);
        let fragment = GitHistoryCommitFragmentV1 {
            commit_oid: commit.clone(),
            fragment_index: 0,
            fragment_count: 1,
            header: Some(GitHistoryCommitHeaderV1 {
                parent_oids: Vec::new(),
                author_name: "A".into(),
                author_email: "a@example.invalid".into(),
                message: "root".into(),
            }),
            changed_paths: vec!["README.md".into()],
        };
        let encoded = encode_history_fragment(&fragment);
        let hash = hex::encode(Sha256::digest(&encoded));
        let manifest = vec![GitHistoryManifestEntryV1 {
            commit_oid: commit.clone(),
            fragment_index: 0,
            encoded_bytes: encoded.len() as u64,
            content_sha256: hash.clone(),
        }];
        let descriptor = GitHistoryDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope: scope.clone(),
            repo_head: commit.clone(),
            object_format: GitObjectFormatV1::Sha1,
            manifest_sha256: history_manifest_sha256(&manifest),
            commit_count: 1,
            fragment_count: 1,
            logical_bytes: encoded.len() as u64,
        };
        let begun = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/git-history/uploads",
                Some(&token),
                Body::from(
                    serde_json::to_vec(&BeginGitHistoryUploadRequestV1 {
                        descriptor: descriptor.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(begun.status(), StatusCode::CREATED);
        let begun: BeginGitHistoryUploadResponseV1 =
            serde_json::from_slice(&to_bytes(begun.into_body(), 64 * 1024).await.unwrap()).unwrap();

        let manifest_response = app
            .clone()
            .oneshot(request(
                "PUT",
                &format!(
                    "/internal/code-source/v1/git-history/uploads/{}/manifest/0",
                    begun.upload_id
                ),
                Some(&token),
                Body::from(
                    serde_json::to_vec(&GitHistoryManifestPageV1 {
                        entries: manifest.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(manifest_response.status(), StatusCode::NO_CONTENT);

        let completed = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/code-source/v1/git-history/uploads/{}/manifest/complete",
                    begun.upload_id
                ),
                Some(&token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(completed.status(), StatusCode::OK);

        let record = Request::builder()
            .method("PUT")
            .uri(format!(
                "/internal/code-source/v1/git-history/uploads/{}/records/{hash}",
                begun.upload_id
            ))
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_LENGTH, encoded.len())
            .body(Body::from(encoded))
            .unwrap();
        let installed = app.clone().oneshot(record).await.unwrap();
        assert_eq!(installed.status(), StatusCode::NO_CONTENT);

        let finalized = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/code-source/v1/git-history/uploads/{}/finalize",
                    begun.upload_id
                ),
                Some(&token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(finalized.status(), StatusCode::ACCEPTED);
        let finalized: bbox_git_source::FinalizeGitHistoryUploadResponseV1 =
            serde_json::from_slice(&to_bytes(finalized.into_body(), 64 * 1024).await.unwrap())
                .unwrap();

        let status = app
            .clone()
            .oneshot(request(
                "GET",
                &finalized.status_url,
                Some(&token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(status.status(), StatusCode::OK);
        let status: GitHistorySourceStatusV1 =
            serde_json::from_slice(&to_bytes(status.into_body(), 64 * 1024).await.unwrap())
                .unwrap();
        assert_eq!(
            status.state,
            bbox_git_source::GitHistorySourceStateV1::Ready
        );

        let probe = app
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/git-history/probe",
                Some(&token),
                Body::from(
                    serde_json::to_vec(&GitHistoryProbeRequestV1 {
                        scope,
                        repo_head: commit,
                        object_format: GitObjectFormatV1::Sha1,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(probe.status(), StatusCode::OK);
        let probe: GitHistoryProbeResponseV1 =
            serde_json::from_slice(&to_bytes(probe.into_body(), 64 * 1024).await.unwrap()).unwrap();
        assert_eq!(
            probe.current.unwrap().source_generation_id,
            finalized.source_generation_id
        );
    }

    #[tokio::test]
    async fn provenance_routes_export_only_observed_inventory_and_persist_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let (state, token, scope) = enabled_state(directory.path());
        let project_id = "p_00000000000000000000000000000001";
        let edges_dir = provenance_edges_dir(&state);
        std::fs::create_dir_all(edges_dir.join("observed")).unwrap();
        let mut metadata = BTreeMap::new();
        metadata.insert("anchor.project_id".into(), project_id.into());
        metadata.insert("anchor.commit_sha_at_edit".into(), "1".repeat(40));
        metadata.insert("anchor.file_path".into(), "src/lib.rs".into());
        metadata.insert("tool.name".into(), "Edit".into());
        let edge = bbox_edge_index::edge_index::Edge {
            source: EntityRef::Transcript {
                provider: "test".into(),
                session_id: "session-1".into(),
                line_offset: 1,
                event_idx: 0,
            },
            kind: "EDITED_FILE".into(),
            target: EntityRef::ProjectFile {
                project_id: project_id.into(),
                rel_path_hash: "path".into(),
                chunk_hash: "a".repeat(64),
                occurrence_idx: 0,
            },
            provenance: EdgeProvenance::Explicit,
            confidence: EdgeConfidence::Heuristic,
            metadata,
            project_id: Some(project_id.into()),
        };
        let lane = edges_dir
            .join("observed")
            .join(format!("{project_id}.jsonl"));
        std::fs::write(
            &lane,
            format!("{}\n", serde_json::to_string(&edge).unwrap()),
        )
        .unwrap();

        let app = router(state.clone()).with_state(state.clone());
        let page = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/provenance/export/page",
                Some(&token),
                Body::from(
                    serde_json::to_vec(&ProvenanceExportPullRequestV1 {
                        scope: scope.clone(),
                        cursor: None,
                        generation: None,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::OK);
        let page: ProvenanceExportPageResponseV1 =
            serde_json::from_slice(&to_bytes(page.into_body(), 128 * 1024).await.unwrap()).unwrap();
        assert_eq!(page.document_count, 1);
        assert_eq!(page.page.documents.len(), 1);
        assert_eq!(page.page.project_id, project_id);

        let receipt = ProvenanceExportReceiptV1 {
            schema_version: SCHEMA_VERSION,
            scope,
            generation: page.page.generation.clone(),
            notes_ref: page.page.notes_ref.clone(),
            document_count: page.document_count,
            ordered_document_commitment: page.ordered_document_commitment.clone(),
            local_notes_tip: "2".repeat(40),
            written: 1,
            unchanged: 0,
        };
        let accepted = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/provenance/export/receipt",
                Some(&token),
                Body::from(serde_json::to_vec(&receipt).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(accepted.status(), StatusCode::NO_CONTENT);
        let stored = state
            .git_sources
            .store()
            .provenance_export_receipt(project_id)
            .unwrap()
            .unwrap();
        assert_eq!(stored.receipt, receipt);

        std::fs::OpenOptions::new()
            .append(true)
            .open(&lane)
            .unwrap()
            .write_all(format!("{}\n", serde_json::to_string(&edge).unwrap()).as_bytes())
            .unwrap();
        let stale = app
            .oneshot(request(
                "POST",
                "/internal/code-source/v1/provenance/export/receipt",
                Some(&token),
                Body::from(serde_json::to_vec(&receipt).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        let metrics = state.git_sources.provenance_export_metrics();
        assert_eq!(metrics.pages_served, 1);
        assert_eq!(metrics.receipts_accepted, 1);
        assert_eq!(metrics.stale_restarts, 1);
    }
}

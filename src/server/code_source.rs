use std::collections::{BTreeMap, BTreeSet};
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bbox_code_source::{
    BeginUploadRequest, ErrorResponse, FinalizeResponse, GenerationState, GenerationStatus,
    ManifestPage, MissingBlobsPage, validate_producer_id, validate_scope,
};
use bbox_code_source_store::{
    ActivationRecord, CodeSourceStore, RetirementRecord, StoreLimits, StoredGeneration,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::ProjectRecord;
use bro_rpc::ServiceToken;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::SharedState;

#[derive(Clone)]
pub(crate) struct ProducerGrant {
    producer_id: String,
    projects: BTreeMap<PublishedScope, String>,
}

struct AuthEntry {
    token: ServiceToken,
    grant: ProducerGrant,
}

struct CodeSourceSnapshot {
    enabled: bool,
    auth: Vec<AuthEntry>,
    store: Arc<CodeSourceStore>,
}

pub(crate) struct CodeSourceRuntime {
    snapshot: parking_lot::RwLock<Arc<CodeSourceSnapshot>>,
    activating_projects: parking_lot::Mutex<BTreeMap<String, bool>>,
}

#[derive(Default)]
pub(crate) struct SourceTransitions {
    cutbacks: Vec<(PublishedScope, String)>,
    activations: Vec<(PublishedScope, String)>,
}

impl CodeSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config, projects: &[ProjectRecord]) -> Result<Self> {
        Ok(Self {
            snapshot: parking_lot::RwLock::new(Arc::new(build_snapshot(config, projects, None)?)),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
        })
    }

    pub(crate) fn reload(
        &self,
        config: &crate::config::Config,
        projects: &[ProjectRecord],
    ) -> Result<SourceTransitions> {
        let previous = self.snapshot.read().clone();
        let replacement = Arc::new(build_snapshot(
            config,
            projects,
            Some(previous.store.clone()),
        )?);
        replacement.store.update_limits(store_limits(config))?;
        let old_assignments = assignment_map(&previous);
        let new_assignments = assignment_map(&replacement);
        let cutbacks = old_assignments
            .iter()
            .filter(|(scope, assignment)| new_assignments.get(*scope) != Some(*assignment))
            .map(|(scope, (project_id, _producer_id))| (scope.clone(), project_id.clone()))
            .collect();
        let activations = new_assignments
            .into_iter()
            .map(|(scope, (project_id, _producer_id))| (scope, project_id))
            .collect();
        *self.snapshot.write() = replacement;
        Ok(SourceTransitions {
            cutbacks,
            activations,
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        let store = Arc::new(
            CodeSourceStore::open(root.join("code-sources"), StoreLimits::default()).unwrap(),
        );
        Self {
            snapshot: parking_lot::RwLock::new(Arc::new(CodeSourceSnapshot {
                enabled: false,
                auth: Vec::new(),
                store,
            })),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
        }
    }

    fn authenticate(&self, candidate: &str) -> Option<ProducerGrant> {
        let snapshot = self.snapshot.read().clone();
        if !snapshot.enabled {
            return None;
        }
        let mut matched = None;
        for entry in &snapshot.auth {
            if entry.token.verify(candidate) {
                matched = Some(entry.grant.clone());
            }
        }
        matched
    }

    fn enabled(&self) -> bool {
        self.snapshot.read().enabled
    }

    pub(crate) fn store(&self) -> Arc<CodeSourceStore> {
        self.snapshot.read().store.clone()
    }

    fn begin_activation(&self, project_id: &str) -> bool {
        let mut projects = self.activating_projects.lock();
        if let Some(pending) = projects.get_mut(project_id) {
            *pending = true;
            false
        } else {
            projects.insert(project_id.to_string(), false);
            true
        }
    }

    fn end_activation(&self, project_id: &str) -> bool {
        self.activating_projects
            .lock()
            .remove(project_id)
            .unwrap_or(false)
    }

    fn assignments(&self) -> Vec<(PublishedScope, String)> {
        let snapshot = self.snapshot.read().clone();
        snapshot
            .auth
            .iter()
            .flat_map(|entry| entry.grant.projects.clone())
            .collect()
    }

    fn assignment_matches(&self, scope: &PublishedScope, project_id: &str) -> bool {
        let snapshot = self.snapshot.read().clone();
        assignment_map(&snapshot)
            .get(scope)
            .is_some_and(|(assigned, _producer_id)| assigned == project_id)
    }

    fn assignment_authorizes(
        &self,
        scope: &PublishedScope,
        project_id: &str,
        producer_id: &str,
    ) -> bool {
        let snapshot = self.snapshot.read().clone();
        assignment_map(&snapshot)
            .get(scope)
            .is_some_and(|(assigned_project, assigned_producer)| {
                assigned_project == project_id && assigned_producer == producer_id
            })
    }
}

fn assignment_map(snapshot: &CodeSourceSnapshot) -> BTreeMap<PublishedScope, (String, String)> {
    snapshot
        .auth
        .iter()
        .flat_map(|entry| {
            entry.grant.projects.iter().map(|(scope, project_id)| {
                (
                    scope.clone(),
                    (project_id.clone(), entry.grant.producer_id.clone()),
                )
            })
        })
        .collect()
}

fn build_snapshot(
    config: &crate::config::Config,
    projects: &[ProjectRecord],
    existing_store: Option<Arc<CodeSourceStore>>,
) -> Result<CodeSourceSnapshot> {
    let limits = store_limits(config);
    if config.code_collection.enabled
        && (limits.max_manifest_files == 0
            || limits.max_manifest_logical_bytes == 0
            || limits.max_open_uploads_per_producer == 0
            || config.code_collection.stale_warning_hours == 0)
    {
        bail!("code-collection limits and stale warning hours must be nonzero");
    }
    let store = if let Some(store) = existing_store {
        store
    } else {
        Arc::new(CodeSourceStore::open(
            config.paths.state_dir.join("code-sources"),
            limits,
        )?)
    };
    if !config.code_collection.enabled {
        return Ok(CodeSourceSnapshot {
            enabled: false,
            auth: Vec::new(),
            store,
        });
    }
    if config.code_collection.producers.is_empty() {
        bail!("enabled code collection requires at least one producer");
    }

    let mut auth = Vec::new();
    let mut producer_ids = BTreeSet::new();
    let mut token_digests = BTreeSet::new();
    let mut assigned_scopes = BTreeSet::new();
    for producer in &config.code_collection.producers {
        validate_producer_id(&producer.producer_id)?;
        if !producer_ids.insert(producer.producer_id.clone()) {
            bail!("duplicate code-collection producer id");
        }
        let token = ServiceToken::load(&producer.token_file).with_context(|| {
            format!("loading code-collection token for {}", producer.producer_id)
        })?;
        let token_digest = Sha256::digest(token.expose_secret().as_bytes());
        if !token_digests.insert(token_digest.to_vec()) {
            bail!("code-collection token values must be unique");
        }
        if producer.scopes.is_empty() {
            bail!("enabled code-collection producer has no scopes");
        }
        let mut resolved = BTreeMap::new();
        for scope in &producer.scopes {
            validate_scope(scope)?;
            if !assigned_scopes.insert(scope.clone()) {
                bail!("code-collection scope is assigned more than once");
            }
            let matching = projects
                .iter()
                .filter(|project| {
                    bbox_indexing::publisher::project_published_scope(project, |root| {
                        bbox_config::config::read_repo_id_inputs(root)
                    })
                    .as_ref()
                        == Some(scope)
                })
                .collect::<Vec<_>>();
            let [project] = matching.as_slice() else {
                if matching.is_empty() {
                    bail!("code-collection scope is not registered");
                }
                bail!("code-collection scope resolves to multiple registered projects");
            };
            resolved.insert(scope.clone(), project.project_id.clone());
        }
        auth.push(AuthEntry {
            token,
            grant: ProducerGrant {
                producer_id: producer.producer_id.clone(),
                projects: resolved,
            },
        });
    }
    Ok(CodeSourceSnapshot {
        enabled: true,
        auth,
        store,
    })
}

fn store_limits(config: &crate::config::Config) -> StoreLimits {
    StoreLimits {
        max_manifest_files: config.code_collection.max_manifest_files,
        max_manifest_logical_bytes: config.code_collection.max_manifest_logical_bytes,
        max_open_uploads_per_producer: config.code_collection.max_open_uploads_per_producer,
        retained_generations: config.code_collection.retained_generations,
        unreferenced_blob_grace_hours: config.code_collection.unreferenced_blob_grace_hours,
    }
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/code-source/v1/uploads",
            post(begin_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/manifest/{page}",
            put(put_manifest_page).layer(DefaultBodyLimit::max(
                bbox_code_source::MAX_MANIFEST_PAGE_BYTES,
            )),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/manifest/complete",
            post(complete_manifest).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/missing",
            get(missing_blobs),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/blobs/{hash}",
            put(put_blob).layer(DefaultBodyLimit::max(
                bbox_code_source::MAX_DOCUMENT_FILE_BYTES as usize,
            )),
        )
        .route(
            "/internal/code-source/v1/uploads/{upload_id}/finalize",
            post(finalize_upload).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/code-source/v1/generations/{generation}/status",
            get(generation_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            authenticate_request,
        ))
}

async fn authenticate_request(
    State(state): State<Arc<SharedState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if !state.code_sources.enabled() {
        return HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "service_disabled",
            "code collection is disabled",
        )
        .into_response();
    }
    let candidate = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(grant) = candidate.and_then(|value| state.code_sources.authenticate(value)) else {
        return HttpError::new(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized")
            .into_response();
    };
    request.extensions_mut().insert(grant);
    next.run(request).await
}

async fn begin_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<BeginUploadRequest>,
) -> Result<impl IntoResponse, HttpError> {
    require_scope(&grant, &request.descriptor.scope)?;
    let store = state.code_sources.store();
    let response =
        blocking(move || store.begin_upload(&grant.producer_id, request.descriptor)).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn put_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, page)): Path<(String, u32)>,
    Json(page_body): Json<ManifestPage>,
) -> Result<StatusCode, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    blocking(move || {
        store.put_manifest_page(&grant.producer_id, &upload_id, page, &page_body.entries)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn complete_manifest(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<Json<MissingBlobsPage>, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let page = blocking(move || store.complete_manifest(&grant.producer_id, &upload_id)).await?;
    Ok(Json(page))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingQuery {
    cursor: Option<String>,
}

async fn missing_blobs(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingBlobsPage>, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let page = blocking(move || {
        store.missing_blobs(&grant.producer_id, &upload_id, query.cursor.as_deref())
    })
    .await?;
    Ok(Json(page))
}

async fn put_blob(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let store = state.code_sources.store();
    require_upload_scope(&store, &grant, &upload_id).await?;
    let expected_size = {
        let store = store.clone();
        let producer_id = grant.producer_id.clone();
        let upload_id = upload_id.clone();
        let hash = hash.clone();
        blocking(move || store.expected_blob_size(&producer_id, &upload_id, &hash)).await?
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
            "blob_size_mismatch",
            "Content-Length does not match the manifest",
        ));
    }

    let temporary = tempfile::NamedTempFile::new_in(store.root()).map_err(HttpError::storage)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen().map_err(HttpError::storage)?);
    let mut stream = body.into_data_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| HttpError::unprocessable("invalid_body", error.to_string()))?;
        written = written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| HttpError::too_large("blob_too_large", "blob body is too large"))?;
        if written > expected_size {
            return Err(HttpError::too_large(
                "blob_too_large",
                "blob body exceeds its manifest size",
            ));
        }
        file.write_all(&chunk).await.map_err(HttpError::storage)?;
    }
    if written != expected_size {
        return Err(HttpError::unprocessable(
            "blob_size_mismatch",
            "blob body is shorter than its manifest size",
        ));
    }
    file.sync_all().await.map_err(HttpError::storage)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(HttpError::storage)?;
    let file = file.into_std().await;
    let producer_id = grant.producer_id;
    blocking(move || store.install_blob(&producer_id, &upload_id, &hash, expected_size, file))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<impl IntoResponse, HttpError> {
    let store = state.code_sources.store();
    let scope = require_upload_scope(&store, &grant, &upload_id).await?;
    let producer_id = grant.producer_id.clone();
    let stored = blocking({
        let store = store.clone();
        move || store.finalize_upload(&producer_id, &upload_id)
    })
    .await?;
    if stored.state == GenerationState::Ready {
        let project_id = require_scope(&grant, &scope)?.to_string();
        schedule_activation(state, scope, project_id);
    }
    let response = FinalizeResponse {
        generation_id: stored.generation_id.clone(),
        status_url: format!(
            "/internal/code-source/v1/generations/{}/status",
            stored.generation_id
        ),
    };
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(generation): Path<String>,
) -> Result<Json<GenerationStatus>, HttpError> {
    bbox_code_source::validate_sha256(&generation)
        .map_err(|error| HttpError::unprocessable("invalid_generation_id", error.to_string()))?;
    let store = state.code_sources.store();
    for scope in grant.projects.keys() {
        let result = {
            let store = store.clone();
            let scope = scope.clone();
            let generation = generation.clone();
            tokio::task::spawn_blocking(move || store.load_generation(&scope, &generation))
                .await
                .map_err(|_| HttpError::storage("generation status task failed"))?
        };
        match result {
            Ok(stored) if stored.producer_id == grant.producer_id => {
                return Ok(Json(status_from_generation(stored)));
            }
            Ok(_) => {}
            Err(error) if store_error_is_not_found(&error) => {}
            Err(error) => return Err(HttpError::from_store(error)),
        }
    }
    Err(HttpError::new(
        StatusCode::NOT_FOUND,
        "generation_not_found",
        "generation not found",
    ))
}

fn schedule_activation(state: Arc<SharedState>, scope: PublishedScope, project_id: String) {
    if !state.code_sources.begin_activation(&project_id) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let mut retry_delay = std::time::Duration::from_secs(1);
        loop {
            let Err(error) = activate_desired_loop(&state, &scope, &project_id) else {
                break;
            };
            let _ = state.code_sources.store().record_health_failure(
                &project_id,
                "activation_failed",
                &error.to_string(),
            );
            tracing::error!(
                project_id = %project_id,
                scope_hash = %bbox_code_source::scope_hash(&scope),
                error = %error,
                retry_seconds = retry_delay.as_secs(),
                "code-source activation failed"
            );
            if !state.code_sources.assignment_matches(&scope, &project_id) {
                break;
            }
            std::thread::sleep(retry_delay);
            retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
        }
        let pending = state.code_sources.end_activation(&project_id);
        if pending
            && let Some((assigned_scope, assigned_project)) = state
                .code_sources
                .assignments()
                .into_iter()
                .find(|(_scope, assigned_project)| assigned_project == &project_id)
        {
            schedule_activation(state, assigned_scope, assigned_project);
            return;
        }
        schedule_cutback_if_owner_changed(state, project_id);
    });
}

fn schedule_cutback_if_owner_changed(state: Arc<SharedState>, project_id: String) {
    let store = state.code_sources.store();
    let Some(activation) = store.load_activation(&project_id).ok().flatten() else {
        return;
    };
    let Ok(generation) = store.find_generation(&activation.generation_id) else {
        return;
    };
    if !state.code_sources.assignment_authorizes(
        &generation.descriptor.scope,
        &project_id,
        &generation.producer_id,
    ) {
        schedule_cutback(state, generation.descriptor.scope, project_id);
    }
}

pub(crate) fn resume_pending_activations(state: Arc<SharedState>) {
    let assignments = state.code_sources.assignments();
    let assigned = assignments.iter().cloned().collect::<BTreeMap<_, _>>();
    for (scope, project_id) in assignments {
        schedule_activation(state.clone(), scope, project_id);
    }
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
        Ok(manifest) => manifest,
        Err(error) => {
            tracing::error!(%error, "code-source manifest recovery is unavailable");
            return;
        }
    };
    let store = state.code_sources.store();
    for (project_id, entry) in &manifest.workspaces {
        if !entry
            .code_source_selector
            .as_deref()
            .is_some_and(|selector| selector.starts_with("collected:"))
        {
            continue;
        }
        let Some(activation) = store.load_activation(project_id).ok().flatten() else {
            tracing::error!(project_id, "active collected source has no recovery record");
            continue;
        };
        let Ok(generation) = store.find_generation(&activation.generation_id) else {
            tracing::error!(
                project_id,
                "active collected source generation is unavailable"
            );
            continue;
        };
        if assigned.get(&generation.descriptor.scope) != Some(project_id) {
            schedule_cutback(
                state.clone(),
                generation.descriptor.scope,
                project_id.clone(),
            );
        }
    }
    match store.activation_records() {
        Ok(records) => {
            for activation in records {
                if assigned
                    .values()
                    .any(|project_id| project_id == &activation.project_id)
                {
                    continue;
                }
                let active_selector = manifest
                    .workspaces
                    .get(&activation.project_id)
                    .cloned()
                    .and_then(|entry| entry.code_source_selector);
                if !active_selector
                    .as_deref()
                    .is_some_and(|selector| selector.starts_with("local:"))
                {
                    continue;
                }
                let retirement = RetirementRecord {
                    version: 1,
                    project_id: activation.project_id.clone(),
                    selector: activation.selector,
                    snapshot_id: activation.snapshot_id,
                    generation_id: Some(activation.generation_id),
                };
                if let Err(error) = store
                    .enqueue_retirement(&retirement)
                    .and_then(|()| store.clear_activation(&activation.project_id))
                    .and_then(|()| {
                        store.clear_health_failure(&activation.project_id, "cutback_pending")
                    })
                {
                    tracing::error!(%error, "recovering completed code-source cutback failed");
                }
            }
        }
        Err(error) => tracing::error!(%error, "loading code-source activations failed"),
    }
    match store.retirement_records() {
        Ok(records) => {
            for record in records {
                spawn_retirement(state.clone(), record, None);
            }
        }
        Err(error) => tracing::error!(%error, "loading code-source retirements failed"),
    }
}

pub(crate) fn apply_source_transitions(state: Arc<SharedState>, transitions: SourceTransitions) {
    for (scope, project_id) in transitions.cutbacks {
        schedule_cutback(state.clone(), scope, project_id);
    }
    for (scope, project_id) in transitions.activations {
        schedule_activation(state.clone(), scope, project_id);
    }
}

pub(crate) fn spawn_store_maintenance(state: &Arc<SharedState>) -> Result<()> {
    let weak = Arc::downgrade(state);
    std::thread::Builder::new()
        .name("blackbox-code-source-maintenance".to_string())
        .spawn(move || {
            let mut tick = 0_u64;
            loop {
                let Some(state) = weak.upgrade() else {
                    break;
                };
                let store = state.code_sources.store();
                match store.expire_uploads(24 * 60 * 60) {
                    Ok(expired) if expired > 0 => {
                        tracing::info!(expired, "expired idle code-source uploads");
                    }
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "code-source upload expiry failed"),
                }
                match store.gc_blobs() {
                    Ok(stats) if stats.reclaimed_blobs > 0 => tracing::info!(
                        blobs = stats.reclaimed_blobs,
                        bytes = stats.reclaimed_bytes,
                        "code-source blob GC reclaimed unreferenced data"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(%error, "code-source blob GC failed"),
                }
                if tick.is_multiple_of(24) {
                    match store.scrub_retained() {
                        Ok(stats) => tracing::info!(
                            blobs = stats.scrubbed_blobs,
                            degraded_generations = stats.degraded_generations,
                            "code-source retained blob scrub complete"
                        ),
                        Err(error) => {
                            tracing::warn!(%error, "code-source retained blob scrub failed")
                        }
                    }
                }
                tick = tick.wrapping_add(1);
                drop(state);
                std::thread::sleep(std::time::Duration::from_secs(60 * 60));
            }
        })
        .context("spawning code-source maintenance thread")?;
    Ok(())
}

fn schedule_cutback(state: Arc<SharedState>, scope: PublishedScope, project_id: String) {
    if !state.code_sources.begin_activation(&project_id) {
        return;
    }
    tokio::task::spawn_blocking(move || {
        let store = state.code_sources.store();
        let mut retry_delay = std::time::Duration::from_secs(1);
        loop {
            match cutback_to_local(&state, &scope, &project_id) {
                Ok(()) => break,
                Err(error) => {
                    let _ = store.mark_cutback_pending(&project_id, &error.to_string());
                    let _ = store.record_health_failure(
                        &project_id,
                        "cutback_pending",
                        &error.to_string(),
                    );
                    tracing::error!(
                        project_id,
                        scope_hash = %bbox_code_source::scope_hash(&scope),
                        error = %error,
                        retry_seconds = retry_delay.as_secs(),
                        "code-source local cutback remains pending"
                    );
                    if state.code_sources.assignment_matches(&scope, &project_id) {
                        break;
                    }
                    std::thread::sleep(retry_delay);
                    retry_delay = (retry_delay * 2).min(std::time::Duration::from_secs(60));
                }
            }
        }
        let _pending = state.code_sources.end_activation(&project_id);
        for (assigned_scope, assigned_project) in state.code_sources.assignments() {
            if assigned_project == project_id {
                schedule_activation(state.clone(), assigned_scope, assigned_project);
            }
        }
    });
}

fn cutback_to_local(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
) -> Result<()> {
    let store = state.code_sources.store();
    let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    let active_is_collected = manifest
        .workspaces
        .get(project_id)
        .and_then(|entry| entry.code_source_selector.as_deref())
        .is_some_and(|selector| selector.starts_with("collected:"));
    if !active_is_collected {
        store.clear_activation(project_id)?;
        return Ok(());
    }
    store.mark_cutback_pending(project_id, "local cutback is staging")?;
    let project = state
        .projects
        .read()
        .list()
        .into_iter()
        .find(|project| project.project_id == project_id)
        .ok_or_else(|| anyhow!("registered project disappeared during local cutback"))?;
    let staged = loop {
        match state
            .index_writer
            .stage_local_generation(project.clone(), scope.clone())
        {
            Ok(staged) => break staged,
            Err(error) if writer_pass_in_progress(&error) => {
                if state.code_sources.assignment_matches(scope, project_id) {
                    bail!("collector assignment returned while local cutback was waiting");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    };
    if state.code_sources.assignment_matches(scope, project_id) {
        bail!("collector assignment returned while local cutback was staging");
    }
    let previous_entry = manifest.workspaces.get(project_id).cloned();
    let previous_view = state.code_read_view.read().clone();
    enqueue_previous_retirement(
        store.as_ref(),
        project_id,
        previous_entry.clone(),
        &staged.selector,
    )?;
    bbox_edge_sidecar::snapshot::activate_local_snapshot_with(
        &edges_dir,
        project_id,
        &scope.repo_id,
        &staged.head_commit,
        &staged.selector,
        &staged.snapshot_id,
        staged.worktree_dirty,
        staged
            .worktree_dirty
            .then_some(staged.dirty_fingerprint.as_str()),
        || {
            let rebuilt = super::routes::build_edge_index_from_shared(state, false);
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), staged.selector.clone());
            index.replace_active_code_selectors(selectors.clone());
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                edge_index: Arc::new(rebuilt),
            });
            Ok(())
        },
    )?;
    if let Some(activation) = store.load_activation(project_id)? {
        if let Ok(generation) = store.find_generation(&activation.generation_id) {
            store.mark_generation_state(
                &generation.descriptor.scope,
                &generation.generation_id,
                GenerationState::Ready,
                None,
            )?;
        }
    }
    schedule_previous_retirement(
        state.clone(),
        project_id,
        previous_entry,
        &staged.selector,
        previous_view,
    )?;
    store.clear_activation(project_id)?;
    store.clear_health_failure(project_id, "cutback_pending")?;
    tracing::info!(
        project_id,
        "code-source project cut back to local ownership"
    );
    Ok(())
}

fn activate_desired_loop(
    state: &Arc<SharedState>,
    scope: &PublishedScope,
    project_id: &str,
) -> Result<()> {
    loop {
        let store = state.code_sources.store();
        let Some(desired) = store.desired_generation(scope)? else {
            return Ok(());
        };
        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired.producer_id)
        {
            return Ok(());
        }
        if desired.state == GenerationState::Active {
            let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
            let expected_snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
                project_id,
                &desired.generation_id,
            );
            let expected_selector = crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired.generation_id,
            );
            let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
            let active_entry = manifest.workspaces.get(project_id);
            let activation = store.load_activation(project_id)?;
            let still_active = active_entry.is_some_and(|entry| {
                entry.code_source_generation.as_deref() == Some(desired.generation_id.as_str())
                    && entry.code_source_selector.as_deref() == Some(expected_selector.as_str())
                    && entry.active_snapshot.as_deref()
                        == Some(
                            bbox_edge_sidecar::snapshot::active_snapshot_rel(
                                project_id,
                                &expected_snapshot,
                            )
                            .as_str(),
                        )
            }) && activation.is_some_and(|activation| {
                activation.generation_id == desired.generation_id
                    && activation.selector == expected_selector
                    && activation.snapshot_id == expected_snapshot
            });
            if still_active {
                return Ok(());
            }
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Ready,
                None,
            )?;
            continue;
        }
        if desired.state == GenerationState::StagingIndex {
            let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
            let active_entry = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?
                .workspaces
                .get(project_id)
                .cloned();
            let expected_snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(
                project_id,
                &desired.generation_id,
            );
            let expected_selector = crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired.generation_id,
            );
            let already_active = active_entry.as_ref().is_some_and(|entry| {
                entry.code_source_generation.as_deref() == Some(desired.generation_id.as_str())
                    && entry.code_source_selector.as_deref() == Some(expected_selector.as_str())
                    && entry.active_snapshot.as_deref()
                        == Some(
                            bbox_edge_sidecar::snapshot::active_snapshot_rel(
                                project_id,
                                &expected_snapshot,
                            )
                            .as_str(),
                        )
            });
            let journal_matches = store
                .load_activation(project_id)?
                .is_some_and(|activation| {
                    activation.generation_id == desired.generation_id
                        && activation.selector == expected_selector
                        && activation.snapshot_id == expected_snapshot
                });
            if already_active && journal_matches {
                store.mark_generation_state(
                    scope,
                    &desired.generation_id,
                    GenerationState::Active,
                    None,
                )?;
                return Ok(());
            }
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Ready,
                None,
            )?;
            continue;
        }
        if desired.state != GenerationState::Ready {
            return Ok(());
        }
        store.mark_generation_state(
            scope,
            &desired.generation_id,
            GenerationState::StagingIndex,
            None,
        )?;
        let project = state
            .projects
            .read()
            .list()
            .into_iter()
            .find(|project| project.project_id == project_id)
            .ok_or_else(|| anyhow!("registered project disappeared during activation"))?;
        let entries = store.load_generation_entries(scope, &desired.generation_id)?;
        let staged = loop {
            match state.index_writer.stage_collected_generation(
                project.clone(),
                desired.descriptor.clone(),
                desired.generation_id.clone(),
                entries.clone(),
                store.clone(),
            ) {
                Ok(staged) => break staged,
                Err(error) if writer_pass_in_progress(&error) => {
                    if !state.code_sources.assignment_authorizes(
                        scope,
                        project_id,
                        &desired.producer_id,
                    ) {
                        store.mark_generation_state(
                            scope,
                            &desired.generation_id,
                            GenerationState::Ready,
                            None,
                        )?;
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
                Err(error) => {
                    store.mark_generation_state(
                        scope,
                        &desired.generation_id,
                        GenerationState::Failed,
                        Some(error.to_string()),
                    )?;
                    store.record_health_failure(
                        project_id,
                        "activation_failed",
                        &error.to_string(),
                    )?;
                    return Err(error);
                }
            }
        };
        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired.producer_id)
        {
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Ready,
                None,
            )?;
            return Ok(());
        }
        let newest = store
            .desired_generation(scope)?
            .ok_or_else(|| anyhow!("desired generation disappeared during activation"))?;
        if newest.generation_id != desired.generation_id {
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Superseded,
                None,
            )?;
            continue;
        }
        store.record_materialization(
            scope,
            &desired.generation_id,
            staged.document_count,
            staged.entity_inventory_sha256.clone(),
        )?;
        store.save_activation(&ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: desired.generation_id.clone(),
            selector: staged.selector.clone(),
            snapshot_id: staged.snapshot_id.clone(),
            document_count: staged.document_count,
            entity_inventory_sha256: store
                .load_generation(scope, &desired.generation_id)?
                .entity_inventory_sha256
                .ok_or_else(|| anyhow!("materialization inventory was not recorded"))?,
            current_chunk_targets: staged.current_chunk_targets.clone().into_iter().collect(),
            activated_unix_secs: unix_now(),
            cutback_pending: false,
            diagnostic: None,
        })?;

        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired.producer_id)
        {
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Ready,
                None,
            )?;
            return Ok(());
        }

        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
        let previous_manifest =
            bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
        let previous_entry = previous_manifest.workspaces.get(project_id).cloned();
        let previous_view = state.code_read_view.read().clone();
        enqueue_previous_retirement(
            store.as_ref(),
            project_id,
            previous_entry.clone(),
            &staged.selector,
        )?;
        bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
            &edges_dir,
            project_id,
            &scope.repo_id,
            &desired.descriptor.head_commit,
            &desired.generation_id,
            &staged.selector,
            &staged.snapshot_id,
            || {
                let rebuilt = super::routes::build_edge_index_from_shared(state, false);
                let index = state.idx.write();
                let mut selectors = index.active_code_selectors();
                selectors.insert(project_id.to_string(), staged.selector.clone());
                index.replace_active_code_selectors(selectors.clone());
                *state.code_read_view.write() = Arc::new(super::CodeReadView {
                    active_selectors: selectors,
                    edge_index: Arc::new(rebuilt),
                });
                Ok(())
            },
        )?;
        tracing::info!(
            project_id,
            generation = %desired.generation_id,
            active_projects = state.code_read_view.read().active_selectors.len(),
            "code-source generation activated"
        );
        store.mark_generation_state(
            scope,
            &desired.generation_id,
            GenerationState::Active,
            None,
        )?;
        store.clear_health_failure(project_id, "activation_failed")?;
        store.clear_health_failure(project_id, "missing_blob_data")?;
        schedule_previous_retirement(
            state.clone(),
            project_id,
            previous_entry,
            &staged.selector,
            previous_view,
        )?;
        return Ok(());
    }
}

fn writer_pass_in_progress(error: &anyhow::Error) -> bool {
    error
        .to_string()
        .contains("a reindex pass is already running")
}

fn selector_retirement_retryable(error: &anyhow::Error) -> bool {
    writer_pass_in_progress(error)
        || error
            .to_string()
            .contains("vector store is still warming up")
}

fn schedule_previous_retirement(
    state: Arc<SharedState>,
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
    previous_view: Arc<super::CodeReadView>,
) -> Result<()> {
    let Some(record) = previous_retirement_record(project_id, previous, active_selector) else {
        return Ok(());
    };
    state.code_sources.store().enqueue_retirement(&record)?;
    spawn_retirement(state, record, Some(previous_view));
    Ok(())
}

fn enqueue_previous_retirement(
    store: &CodeSourceStore,
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
) -> Result<()> {
    if let Some(record) = previous_retirement_record(project_id, previous, active_selector) {
        store.enqueue_retirement(&record)?;
    }
    Ok(())
}

fn previous_retirement_record(
    project_id: &str,
    previous: Option<bbox_edge_sidecar::manifest::WorkspaceIndexEntry>,
    active_selector: &str,
) -> Option<RetirementRecord> {
    let Some(previous) = previous else {
        return None;
    };
    let (Some(selector), Some(snapshot_id)) = (
        previous.code_source_selector,
        previous
            .active_snapshot
            .as_deref()
            .and_then(|path| path.rsplit('/').next())
            .map(str::to_string),
    ) else {
        return None;
    };
    if selector == active_selector {
        return None;
    }
    Some(RetirementRecord {
        version: 1,
        project_id: project_id.to_string(),
        selector,
        snapshot_id,
        generation_id: previous
            .code_source_generation
            .filter(|value| value != "local"),
    })
}

fn spawn_retirement(
    state: Arc<SharedState>,
    record: RetirementRecord,
    previous_view: Option<Arc<super::CodeReadView>>,
) {
    if let Err(error) = std::thread::Builder::new()
        .name("blackbox-code-source-retirement".to_string())
        .spawn(move || {
            if let Some(previous_view) = previous_view {
                while Arc::strong_count(&previous_view) > 1 {
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
            let edges_dir = crate::edge_index::edges_dir_from_bro_store(&state.store_dir);
            let active_selector =
                match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
                    Ok(manifest) => manifest
                        .workspaces
                        .get(&record.project_id)
                        .cloned()
                        .and_then(|entry| entry.code_source_selector),
                    Err(error) => {
                        let _ = state.code_sources.store().record_health_failure(
                            &record.project_id,
                            "retirement_failed",
                            &error.to_string(),
                        );
                        tracing::error!(%error, "code-source retirement authority read failed");
                        return;
                    }
                };
            if active_selector.as_deref() == Some(record.selector.as_str()) {
                return;
            }
            loop {
                let selector_is_active =
                    match bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir) {
                        Ok(manifest) => manifest.workspaces.values().any(|entry| {
                            entry.code_source_selector.as_deref() == Some(record.selector.as_str())
                        }),
                        Err(error) => {
                            let _ = state.code_sources.store().record_health_failure(
                                &record.project_id,
                                "retirement_failed",
                                &error.to_string(),
                            );
                            tracing::error!(%error, "code-source retirement authority read failed");
                            return;
                        }
                    };
                if selector_is_active {
                    return;
                }
                match state
                    .index_writer
                    .retire_code_selector(record.selector.clone())
                {
                    Ok(retired) => {
                        tracing::info!(
                            project_id = %record.project_id,
                            selector = %record.selector,
                            document_count = retired.document_count,
                            "retired inactive code-source selector"
                        );
                        let cleanup = bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
                            let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(
                                &edges_dir,
                            )?;
                            let selector_is_active = manifest.workspaces.values().any(|entry| {
                                entry.code_source_selector.as_deref()
                                    == Some(record.selector.as_str())
                            });
                            let snapshot_is_active = manifest
                                .workspaces
                                .get(&record.project_id)
                                .and_then(|entry| entry.active_snapshot.as_deref())
                                == Some(
                                    bbox_edge_sidecar::snapshot::active_snapshot_rel(
                                        &record.project_id,
                                        &record.snapshot_id,
                                    )
                                    .as_str(),
                                );
                            if selector_is_active || snapshot_is_active {
                                return Ok(false);
                            }
                            if !record.snapshot_id.contains('/')
                                && !record.snapshot_id.contains('\\')
                                && record.snapshot_id != "."
                                && record.snapshot_id != ".."
                            {
                                let snapshot = bbox_edge_sidecar::snapshot::snapshot_dir(
                                    &edges_dir,
                                    &record.project_id,
                                    &record.snapshot_id,
                                );
                                if snapshot.is_dir() {
                                    std::fs::remove_dir_all(&snapshot)?;
                                }
                            }
                            Ok(true)
                        });
                        match cleanup {
                            Ok(true) => {}
                            Ok(false) => return,
                            Err(error) => {
                                let _ = state.code_sources.store().record_health_failure(
                                    &record.project_id,
                                    "retirement_failed",
                                    &error.to_string(),
                                );
                                tracing::error!(%error, "retired snapshot cleanup failed");
                                return;
                            }
                        }
                        let store = state.code_sources.store();
                        let generation_is_still_active = match store
                            .load_activation(&record.project_id)
                        {
                            Ok(activation) => activation.is_some_and(|activation| {
                                record.generation_id.as_deref()
                                    == Some(activation.generation_id.as_str())
                            }),
                            Err(error) => {
                                let _ = store.record_health_failure(
                                    &record.project_id,
                                    "retirement_failed",
                                    &error.to_string(),
                                );
                                tracing::error!(%error, "retirement activation read failed");
                                return;
                            }
                        };
                        if let Some(generation_id) = &record.generation_id
                            && !generation_is_still_active
                            && let Ok(generation) = store.find_generation(generation_id)
                        {
                            let _ = store.mark_generation_state(
                                &generation.descriptor.scope,
                                generation_id,
                                GenerationState::Superseded,
                                None,
                            );
                        }
                        let _ = store.clear_health_failure(
                            &record.project_id,
                            "retirement_failed",
                        );
                        if let Err(error) = store.complete_retirement(&record.selector) {
                            tracing::warn!(%error, "completing code-source retirement record failed");
                        }
                        drop(retired);
                        return;
                    }
                    Err(error) if selector_retirement_retryable(&error) => {
                        std::thread::sleep(std::time::Duration::from_secs(1));
                    }
                    Err(error) => {
                        let _ = state.code_sources.store().record_health_failure(
                            &record.project_id,
                            "retirement_failed",
                            &error.to_string(),
                        );
                        tracing::error!(%error, "code-source selector retirement failed");
                        return;
                    }
                }
            }
        })
    {
        tracing::error!(%error, "spawning code-source retirement thread failed");
    }
}

async fn require_upload_scope(
    store: &Arc<CodeSourceStore>,
    grant: &ProducerGrant,
    upload_id: &str,
) -> Result<PublishedScope, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let upload_id = upload_id.to_string();
    let scope = tokio::task::spawn_blocking(move || store.upload_scope(&producer_id, &upload_id))
        .await
        .map_err(|_| HttpError::storage("upload lookup task failed"))?
        .map_err(|error| {
            if store_error_is_not_found(&error) {
                HttpError::new(StatusCode::NOT_FOUND, "not_found", "resource not found")
            } else {
                HttpError::from_store(error)
            }
        })?;
    require_scope(grant, &scope)?;
    Ok(scope)
}

fn require_scope<'a>(
    grant: &'a ProducerGrant,
    scope: &PublishedScope,
) -> Result<&'a str, HttpError> {
    grant
        .projects
        .get(scope)
        .map(String::as_str)
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::FORBIDDEN,
                "scope_forbidden",
                "scope is not authorized for this producer",
            )
        })
}

fn status_from_generation(stored: StoredGeneration) -> GenerationStatus {
    GenerationStatus {
        generation_id: stored.generation_id,
        state: stored.state,
        file_count: stored.descriptor.file_count,
        logical_bytes: stored.descriptor.logical_bytes,
        diagnostic: stored.diagnostic,
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::storage(anyhow!("blocking task failed")))?
        .map_err(HttpError::from_store)
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

    fn storage(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "code-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "code-source storage is unavailable",
        )
    }

    fn from_store(error: anyhow::Error) -> Self {
        let message = error.to_string();
        if error
            .chain()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        {
            return Self::storage(error);
        }
        if message.contains("maximum number of open uploads") {
            return Self::new(StatusCode::TOO_MANY_REQUESTS, "upload_limit", message);
        }
        if message.contains("exceeds configured limit")
            || message.contains("exceeds entry cap")
            || message.contains("exceeds byte cap")
        {
            return Self::too_large("limit_exceeded", message);
        }
        if message.contains("not found") {
            return Self::new(StatusCode::NOT_FOUND, "not_found", "resource not found");
        }
        if message.contains("ownership mismatch") || message.contains("another producer") {
            return Self::new(StatusCode::FORBIDDEN, "scope_forbidden", "forbidden");
        }
        Self::unprocessable("invalid_code_source_state", message)
    }
}

fn store_error_is_not_found(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    })
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

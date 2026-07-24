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
    BeginUploadRequest, ContractError, ErrorResponse, FinalizeResponse, GenerationState,
    GenerationStatus, ManifestPage, MissingBlobsPage, validate_producer_id, validate_scope,
};
use bbox_code_source_store::{
    ActivationRecord, CodeSourceStore, CollisionRetirementWorkV1, RetirementRecord, StoreLimits,
    StoreRequestError, StoredGeneration,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_core::project_record::ProjectRecord;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector,
};
use bro_rpc::ServiceToken;
use futures::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::SharedState;

const UPLOAD_BODY_TEMP_PREFIX: &str = ".upload-body-";
const UPLOAD_BODY_TEMP_SUFFIX: &str = ".tmp";

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
    checkout_access: Arc<CheckoutAccessBroker>,
}

#[derive(Default)]
pub(crate) struct SourceTransitions {
    cutbacks: Vec<(PublishedScope, String)>,
    activations: Vec<(PublishedScope, String)>,
}

impl CodeSourceRuntime {
    pub(crate) fn open(
        config: &crate::config::Config,
        projects: &[ProjectRecord],
        checkout_access: Arc<CheckoutAccessBroker>,
    ) -> Result<Self> {
        Ok(Self {
            snapshot: parking_lot::RwLock::new(Arc::new(build_snapshot(
                config,
                projects,
                None,
                &checkout_access,
            )?)),
            activating_projects: parking_lot::Mutex::new(BTreeMap::new()),
            checkout_access,
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
            &self.checkout_access,
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
            checkout_access: Arc::new(CheckoutAccessBroker::new(
                Arc::new(bbox_indexing::checkout_access::DenyCheckoutAccess),
                bbox_indexing::checkout_access::CheckoutAccessObservations::in_memory(),
            )),
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
    checkout_access: &CheckoutAccessBroker,
) -> Result<CodeSourceSnapshot> {
    let limits = store_limits(config);
    if config.code_collection.enabled
        && (limits.max_manifest_files == 0
            || limits.max_manifest_logical_bytes == 0
            || limits.max_open_uploads_per_producer == 0
            || limits.max_migration_survivor_rows == 0
            || limits.max_migration_survivor_bytes == 0
            || config.code_collection.stale_warning_hours == 0)
    {
        bail!("code-collection limits and stale warning hours must be nonzero");
    }
    let store = if let Some(store) = existing_store {
        store
    } else {
        let store = Arc::new(CodeSourceStore::open(
            config.paths.state_dir.join("code-sources"),
            limits,
        )?);
        store
    };
    if !config.code_collection.enabled {
        return Ok(CodeSourceSnapshot {
            enabled: false,
            auth: Vec::new(),
            store,
        });
    }
    reap_upload_body_tempfiles(store.root())?;
    if config.code_collection.producers.is_empty() {
        bail!("enabled code collection requires at least one producer");
    }

    let project_scopes = projects
        .iter()
        .map(|project| {
            let lease = checkout_access
                .acquire(CheckoutAccessRequest {
                    project_id: project.project_id.clone(),
                    attachment: CheckoutAttachmentSelector::Selected,
                    expected_scope: None,
                    kind: CheckoutAccessKind::PublisherConfigTreeRead,
                    intent: CheckoutAccessIntent::Read,
                    source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
                })
                .map_err(anyhow::Error::new)?;
            let scope = lease.published_scope().cloned();
            checkout_access
                .revalidate(&lease)
                .map_err(anyhow::Error::new)?;
            Ok::<_, anyhow::Error>((project, scope))
        })
        .collect::<Result<Vec<_>>>()?;
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
            let matching = project_scopes
                .iter()
                .filter(|(_, project_scope)| project_scope.as_ref() == Some(scope))
                .map(|(project, _)| *project)
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
        max_migration_survivor_rows: config.code_collection.max_migration_survivor_rows,
        max_migration_survivor_bytes: config.code_collection.max_migration_survivor_bytes,
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

fn reap_upload_body_tempfiles(store_root: &std::path::Path) -> Result<u64> {
    let mut reaped = 0_u64;
    for entry in std::fs::read_dir(store_root)
        .with_context(|| format!("reading code-source store root {}", store_root.display()))?
    {
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
        let directory = std::fs::File::open(store_root)?;
        directory.sync_all()?;
    }
    Ok(reaped)
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
                "activation failed; inspect daemon logs",
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
    match retirement_records_for_recovery(&store) {
        Ok(records) => {
            for record in records {
                spawn_retirement(state.clone(), record, None, RetirementCompletion::Ordinary);
            }
        }
        Err(error) => tracing::error!(%error, "loading code-source retirements failed"),
    }
    match collision_retirement_tasks_for_recovery(&store) {
        Ok(tasks) => {
            for task in tasks {
                match task {
                    CollisionRetirementRecoveryTask::Exact { work, selector } => {
                        let record = RetirementRecord {
                            version: 1,
                            project_id: work.project_id.to_string(),
                            selector,
                            snapshot_id: work.snapshot_id.clone(),
                            generation_id: Some(work.generation_id.clone()),
                        };
                        let completion = RetirementCompletion::Collision {
                            project_id: work.project_id,
                            generation_id: work.generation_id,
                        };
                        spawn_retirement(state.clone(), record, None, completion);
                    }
                    CollisionRetirementRecoveryTask::Selectorless { work } => {
                        spawn_selectorless_collision_retirement(state.clone(), work);
                    }
                }
            }
        }
        Err(error) => tracing::error!(%error, "loading collision retirement work failed"),
    }
}

fn retirement_records_for_recovery(store: &CodeSourceStore) -> Result<Vec<RetirementRecord>> {
    store.retirement_records()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CollisionRetirementRecoveryTask {
    Exact {
        work: CollisionRetirementWorkV1,
        selector: String,
    },
    Selectorless {
        work: CollisionRetirementWorkV1,
    },
}

fn collision_retirement_tasks_for_recovery(
    store: &CodeSourceStore,
) -> Result<Vec<CollisionRetirementRecoveryTask>> {
    store.reconcile_collision_retirements()?;
    store
        .collision_retirement_work_records()?
        .into_iter()
        .map(|work| {
            if let Some(selector) = work.exact_selector().map(str::to_string) {
                Ok(CollisionRetirementRecoveryTask::Exact { work, selector })
            } else {
                Ok(CollisionRetirementRecoveryTask::Selectorless { work })
            }
        })
        .collect()
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
                    Ok(stats) if stats.reclaimed_blobs > 0 || stats.reclaimed_generations > 0 => {
                        tracing::info!(
                            blobs = stats.reclaimed_blobs,
                            bytes = stats.reclaimed_bytes,
                            generations = stats.reclaimed_generations,
                            "code-source GC reclaimed unreferenced data"
                        )
                    }
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
                    let _ = store
                        .mark_cutback_pending(&project_id, "cutback failed; inspect daemon logs");
                    let _ = store.record_health_failure(
                        &project_id,
                        "cutback_pending",
                        "cutback failed; inspect daemon logs",
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
    ensure_selector_staging_available(
        store.as_ref(),
        &bbox_code_source::local_selector(project_id),
    )?;
    let cutback_deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let staged = loop {
        match state
            .index_writer
            .stage_local_generation(project.clone(), scope.clone())
        {
            Ok(staged) => break staged,
            Err(error) if writer_pass_in_progress(&error) => {
                if std::time::Instant::now() >= cutback_deadline {
                    bail!("local cutback timed out waiting for the index writer");
                }
                if !state
                    .projects
                    .read()
                    .list()
                    .iter()
                    .any(|project| project.project_id == project_id)
                {
                    bail!("registered project disappeared while local cutback was waiting");
                }
                if state.code_sources.assignment_matches(scope, project_id) {
                    bail!("collector assignment returned while local cutback was waiting");
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(error) => return Err(error),
        }
    };
    if state.code_sources.assignment_matches(scope, project_id) {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        bail!("collector assignment returned while local cutback was staging");
    }
    staged.begin_publication()?;
    if let Err(error) = state
        .index_writer
        .verify_code_selector_document_count(&staged.selector, staged.document_count)
    {
        schedule_unactivated_retirement(state, project_id, &staged, None)?;
        return Err(error);
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
        scope.repo_id(),
        &staged.head_commit,
        &staged.selector,
        &staged.snapshot_id,
        staged.worktree_dirty,
        staged
            .worktree_dirty
            .then_some(staged.dirty_fingerprint.as_str()),
        || {
            let rebuilt = super::routes::build_edge_index_from_shared(state, false)?;
            let index = state.idx.write();
            let mut selectors = index.active_code_selectors();
            selectors.insert(project_id.to_string(), staged.selector.clone());
            index.replace_active_code_selectors(selectors.clone());
            *state.code_read_view.write() = Arc::new(super::CodeReadView {
                active_selectors: selectors,
                searcher: index.searcher(),
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
        ensure_selector_staging_available(
            store.as_ref(),
            &crate::index::project_files::collected_materialization_selector(
                project_id,
                &desired.generation_id,
            ),
        )?;
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
                        Some("activation failed; inspect daemon logs".into()),
                    )?;
                    store.record_health_failure(
                        project_id,
                        "activation_failed",
                        "activation failed; inspect daemon logs",
                    )?;
                    return Err(error);
                }
            }
        };
        if !state
            .code_sources
            .assignment_authorizes(scope, project_id, &desired.producer_id)
        {
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired.generation_id.clone()),
            )?;
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
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired.generation_id.clone()),
            )?;
            store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Superseded,
                None,
            )?;
            continue;
        }
        staged.begin_publication()?;
        if let Err(error) = state
            .index_writer
            .verify_code_selector_document_count(&staged.selector, staged.document_count)
        {
            let retirement = enqueue_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired.generation_id.clone()),
            )?;
            let mark_result = store.mark_generation_state(
                scope,
                &desired.generation_id,
                GenerationState::Failed,
                Some("staged document verification failed; inspect daemon logs".into()),
            );
            spawn_retirement(
                state.clone(),
                retirement,
                None,
                RetirementCompletion::Ordinary,
            );
            mark_result?;
            return Err(error);
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
            schedule_unactivated_retirement(
                state,
                project_id,
                &staged,
                Some(desired.generation_id.clone()),
            )?;
            store.clear_activation(project_id)?;
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
            scope.repo_id(),
            &desired.descriptor.head_commit,
            &desired.generation_id,
            &staged.selector,
            &staged.snapshot_id,
            || {
                let rebuilt = super::routes::build_edge_index_from_shared(state, false)?;
                let index = state.idx.write();
                let mut selectors = index.active_code_selectors();
                selectors.insert(project_id.to_string(), staged.selector.clone());
                index.replace_active_code_selectors(selectors.clone());
                *state.code_read_view.write() = Arc::new(super::CodeReadView {
                    active_selectors: selectors,
                    searcher: index.searcher(),
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
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<
                bbox_indexing::index::writer_actor::IndexWriterRetryableError,
            >(),
            Some(
                bbox_indexing::index::writer_actor::IndexWriterRetryableError::ReindexPassInProgress
            )
        )
    })
}

fn selector_retirement_retryable(error: &anyhow::Error) -> bool {
    writer_pass_in_progress(error)
        || error.chain().any(|cause| {
            matches!(
            cause.downcast_ref::<bbox_indexing::index::writer_actor::IndexWriterRetryableError>(),
            Some(bbox_indexing::index::writer_actor::IndexWriterRetryableError::VectorStoreWarming)
        )
        })
}

const SELECTOR_RETIREMENT_RETRY_LIMIT: u32 = 8;
const SELECTOR_RETIREMENT_REDRIVE_DELAY: std::time::Duration = std::time::Duration::from_secs(60);

fn take_selector_retirement_retry(
    attempts: &mut u32,
    delay: &mut std::time::Duration,
) -> Option<std::time::Duration> {
    if *attempts >= SELECTOR_RETIREMENT_RETRY_LIMIT {
        return None;
    }
    *attempts += 1;
    let current = *delay;
    *delay = (*delay * 2).min(std::time::Duration::from_secs(30));
    Some(current)
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
    spawn_retirement(
        state,
        record,
        Some(previous_view),
        RetirementCompletion::Ordinary,
    );
    Ok(())
}

fn schedule_unactivated_retirement(
    state: &Arc<SharedState>,
    project_id: &str,
    staged: &crate::index::project_files::CollectedIndexResult,
    generation_id: Option<String>,
) -> Result<()> {
    let record = enqueue_unactivated_retirement(state, project_id, staged, generation_id)?;
    spawn_retirement(state.clone(), record, None, RetirementCompletion::Ordinary);
    Ok(())
}

fn enqueue_unactivated_retirement(
    state: &Arc<SharedState>,
    project_id: &str,
    staged: &crate::index::project_files::CollectedIndexResult,
    generation_id: Option<String>,
) -> Result<RetirementRecord> {
    let record = RetirementRecord {
        version: 1,
        project_id: project_id.to_string(),
        selector: staged.selector.clone(),
        snapshot_id: staged.snapshot_id.clone(),
        generation_id,
    };
    state.code_sources.store().enqueue_retirement(&record)?;
    Ok(record)
}

fn ensure_selector_staging_available(store: &CodeSourceStore, selector: &str) -> Result<()> {
    // The per-project activation lane is the sole runtime enqueuer for its
    // selectors. A durable queue row therefore separates two staging epochs.
    if store.retirement_pending(selector)? {
        bail!("code-source selector retirement remains queued before staging");
    }
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
    let previous = previous?;
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
    completion: RetirementCompletion,
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
                            "retirement failed; inspect daemon logs",
                        );
                        tracing::error!(%error, "code-source retirement authority read failed");
                        return;
                    }
                };
            if active_selector.as_deref() == Some(record.selector.as_str()) {
                return;
            }
            let mut retry_attempts = 0;
            let mut retry_delay = std::time::Duration::from_secs(1);
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
                                "retirement failed; inspect daemon logs",
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
                        if let Err(error) = retired.begin_cleanup() {
                            let _ = state.code_sources.store().record_health_failure(
                                &record.project_id,
                                "retirement_failed",
                                "retirement cleanup hold expired; work remains queued",
                            );
                            tracing::error!(%error, "selector retirement cleanup hold expired");
                            return;
                        }
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
                                    "retirement failed; inspect daemon logs",
                                );
                                tracing::error!(%error, "retired snapshot cleanup failed");
                                return;
                            }
                        }
                        let store = state.code_sources.store();
                        match &completion {
                            RetirementCompletion::Collision {
                                project_id,
                                generation_id,
                            } => {
                                if let Err(error) = repair_and_complete_collision_retirement(
                                    &store,
                                    project_id,
                                    generation_id,
                                ) {
                                    let _ = store.record_health_failure(
                                        project_id.as_str(),
                                        "retirement_failed",
                                        "retirement failed; inspect daemon logs",
                                    );
                                    tracing::error!(
                                        project_id = %project_id,
                                        generation_id,
                                        %error,
                                        "collision retirement generation repair failed"
                                    );
                                    return;
                                }
                            }
                            RetirementCompletion::Ordinary => {
                                if let Err(error) = store.complete_retirement(&record) {
                                    let _ = store.record_health_failure(
                                        &record.project_id,
                                        "retirement_failed",
                                        "retirement failed; inspect daemon logs",
                                    );
                                    tracing::error!(
                                        %error,
                                        "code-source retirement completion failed"
                                    );
                                    return;
                                }
                            }
                        }
                        let _ = store.clear_health_failure(
                            &record.project_id,
                            "retirement_failed",
                        );
                        drop(retired);
                        return;
                    }
                    Err(error) if selector_retirement_retryable(&error) => {
                        let Some(delay) = take_selector_retirement_retry(
                            &mut retry_attempts,
                            &mut retry_delay,
                        ) else {
                            let _ = state.code_sources.store().record_health_failure(
                                &record.project_id,
                                "retirement_failed",
                                "retirement retry budget exhausted; work remains queued",
                            );
                            tracing::error!(
                                %error,
                                attempts = retry_attempts,
                                redrive_secs = SELECTOR_RETIREMENT_REDRIVE_DELAY.as_secs(),
                                "code-source selector retirement retry budget exhausted; scheduling in-process redrive"
                            );
                            std::thread::sleep(SELECTOR_RETIREMENT_REDRIVE_DELAY);
                            retry_attempts = 0;
                            retry_delay = std::time::Duration::from_secs(1);
                            continue;
                        };
                        std::thread::sleep(delay);
                    }
                    Err(error) => {
                        let _ = state.code_sources.store().record_health_failure(
                            &record.project_id,
                            "retirement_failed",
                            "retirement failed; inspect daemon logs",
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

#[derive(Debug, Clone)]
enum RetirementCompletion {
    Ordinary,
    Collision {
        project_id: ProjectId,
        generation_id: String,
    },
}

fn repair_and_complete_collision_retirement(
    store: &CodeSourceStore,
    project_id: &ProjectId,
    generation_id: &str,
) -> Result<()> {
    store
        .repair_and_complete_collision_retirement(project_id, generation_id)
        .context("repairing and completing collision retirement")
}

fn spawn_selectorless_collision_retirement(
    state: Arc<SharedState>,
    work: CollisionRetirementWorkV1,
) {
    if let Err(error) = std::thread::Builder::new()
        .name("blackbox-code-source-collision-retirement".to_string())
        .spawn(move || {
            let store = state.code_sources.store();
            if let Err(error) = repair_and_complete_collision_retirement(
                &store,
                &work.project_id,
                &work.generation_id,
            ) {
                let _ = store.record_health_failure(
                    work.project_id.as_str(),
                    "retirement_failed",
                    "retirement failed; inspect daemon logs",
                );
                tracing::error!(
                    project_id = %work.project_id,
                    generation_id = %work.generation_id,
                    %error,
                    "selectorless collision retirement generation repair failed"
                );
                return;
            }
            let _ = store.clear_health_failure(work.project_id.as_str(), "retirement_failed");
        })
    {
        tracing::error!(%error, "spawning selectorless collision retirement thread failed");
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
    let diagnostic = stored.diagnostic.map(|_| {
        match stored.state {
            GenerationState::MissingBlobData => {
                "retained blob data is unavailable; recollect this generation"
            }
            GenerationState::Failed => "generation processing failed; inspect daemon logs",
            _ => "generation processing requires operator attention",
        }
        .to_string()
    });
    GenerationStatus {
        generation_id: stored.generation_id,
        state: stored.state,
        file_count: stored.descriptor.file_count,
        logical_bytes: stored.descriptor.logical_bytes,
        diagnostic,
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

    fn too_many_requests(code: &str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::TOO_MANY_REQUESTS, code, message)
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
            return match contract {
                ContractError::FileTooLarge { .. }
                | ContractError::TooManyFiles { .. }
                | ContractError::TooManyBytes { .. } => Self::too_large(
                    "limit_exceeded",
                    "code-source input exceeds an enforced limit",
                ),
                ContractError::UnsupportedSchema(_)
                | ContractError::WalkerPolicyMismatch { .. } => Self::unprocessable(
                    "unsupported_contract",
                    "code-source contract version is unsupported",
                ),
                _ => Self::unprocessable(
                    "invalid_code_source_input",
                    "code-source input violates the collection contract",
                ),
            };
        }
        if let Some(request) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<StoreRequestError>())
        {
            return match request {
                StoreRequestError::LimitExceeded => Self::too_large(
                    "limit_exceeded",
                    "code-source input exceeds an enforced limit",
                ),
                StoreRequestError::TooManyOpenUploads => Self::too_many_requests(
                    "upload_limit_reached",
                    "producer has too many open uploads",
                ),
                StoreRequestError::InvalidState => Self::unprocessable(
                    "invalid_upload_state",
                    "upload is not in the required state",
                ),
                StoreRequestError::InvalidInput => {
                    Self::unprocessable("invalid_code_source_input", "code-source input is invalid")
                }
            };
        }
        Self::storage(error)
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::fs;
    use std::path::Path;

    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use bbox_code_source::{
        BeginUploadResponse, GenerationDescriptor, ManifestEntry, SCHEMA_VERSION,
        WALKER_POLICY_VERSION, dirty_fingerprint, generation_id as compute_generation_id,
        manifest_sha256, source_selector,
    };
    use bbox_code_source_store::{
        CodeSourceStorePaths, CollisionRetirementEntryV1, CollisionRetirementLifecycleStateV1,
        CollisionRetirementLifecycleV1, CollisionRetirementSelectorEvidenceV1,
        CollisionRetirementWorkV1, StoredGenerationV2,
        decode_collision_retirement_pending_for_migration,
        decode_stored_generation_v2_for_migration,
        encode_collision_retirement_pending_for_migration,
        encode_stored_generation_v2_for_migration,
    };
    use bbox_config::config::CodeCollectionProducerConfig;
    use bbox_corpus_core::project_catalog::ProjectId;
    use bbox_indexing::checkout_access::{
        CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError,
        CheckoutAccessErrorCode, CheckoutAccessObservations, CheckoutAttachmentStatus,
    };
    use tower::ServiceExt;

    use super::*;

    #[derive(Clone)]
    struct SnapshotAuthority {
        candidates: BTreeMap<String, CheckoutAccessCandidate>,
    }

    impl CheckoutAccessAuthority for SnapshotAuthority {
        fn resolve(
            &self,
            request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.candidates
                .get(&request.project_id)
                .cloned()
                .ok_or_else(|| {
                    CheckoutAccessError::new(
                        CheckoutAccessErrorCode::AttachmentNotFound,
                        "test project has no checkout candidate",
                    )
                })
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            Ok(())
        }
    }

    fn empty_generation_descriptor(scope: PublishedScope, head: &str) -> GenerationDescriptor {
        GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope,
            head_commit: head.to_string(),
            dirty_fingerprint: dirty_fingerprint(head, &[]),
            manifest_sha256: manifest_sha256(&[]),
            file_count: 0,
            logical_bytes: 0,
        }
    }

    fn git(root: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn write_service_token(path: &Path, secret: char) {
        fs::write(path, secret.to_string().repeat(64)).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    fn snapshot_project(
        root: &Path,
        project_id: &str,
        scope: &PublishedScope,
    ) -> (ProjectRecord, CheckoutAccessCandidate) {
        let project_root = root.join(project_id);
        fs::create_dir_all(&project_root).unwrap();
        let project_root = project_root.canonicalize().unwrap();
        (
            ProjectRecord {
                project_id: project_id.to_string(),
                repo_id: Some(scope.repo_id().to_string()),
                canonical_path: project_root.to_string_lossy().into_owned(),
                registered_at: "2026-01-01T00:00:00Z".into(),
                is_git_repo: true,
                languages: BTreeSet::new(),
                aliases: BTreeSet::new(),
            },
            CheckoutAccessCandidate {
                project_id: project_id.to_string(),
                attachment_id: format!("attachment-{project_id}"),
                checkout_id: format!("checkout-{project_id}"),
                published_scope: Some(scope.clone()),
                branch_ref: Some("refs/heads/main".into()),
                checkout_root: project_root.clone(),
                project_root,
                status: CheckoutAttachmentStatus::Active,
                capabilities: BTreeSet::from([CheckoutAccessKind::PublisherConfigTreeRead]),
                lifetime_guard: None,
            },
        )
    }

    fn snapshot_broker(candidates: Vec<CheckoutAccessCandidate>) -> CheckoutAccessBroker {
        CheckoutAccessBroker::new(
            Arc::new(SnapshotAuthority {
                candidates: candidates
                    .into_iter()
                    .map(|candidate| (candidate.project_id.clone(), candidate))
                    .collect(),
            }),
            CheckoutAccessObservations::in_memory(),
        )
    }

    fn assert_snapshot_rejected(
        base: &crate::config::Config,
        producers: Vec<CodeCollectionProducerConfig>,
        projects: &[ProjectRecord],
        store: Arc<CodeSourceStore>,
        broker: &CheckoutAccessBroker,
        expected: &str,
    ) {
        let mut config = base.clone();
        config.code_collection.enabled = true;
        config.code_collection.producers = producers;
        let error = build_snapshot(&config, projects, Some(store), broker)
            .err()
            .expect("invalid enabled code-source configuration must fail closed");
        assert_eq!(error.to_string(), expected);
    }

    fn install_test_assignment(
        state: &Arc<SharedState>,
        producer_id: &str,
        scope: &PublishedScope,
        project_id: &str,
    ) {
        let store = state.code_sources.store();
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            enabled: true,
            auth: vec![AuthEntry {
                token: ServiceToken::parse("a".repeat(64)).unwrap(),
                grant: ProducerGrant {
                    producer_id: producer_id.to_string(),
                    projects: BTreeMap::from([(scope.clone(), project_id.to_string())]),
                },
            }],
            store,
        });
    }

    fn enabled_http_state(
        root: &std::path::Path,
        scope: &PublishedScope,
    ) -> (Arc<SharedState>, String) {
        let state = Arc::new(SharedState::for_test(root));
        let token_secret = "a".repeat(64);
        let token = ServiceToken::parse(token_secret.clone()).unwrap();
        let store = state.code_sources.store();
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            enabled: true,
            auth: vec![AuthEntry {
                token,
                grant: ProducerGrant {
                    producer_id: "http-test-producer".into(),
                    projects: BTreeMap::from([(scope.clone(), "http-test-project".into())]),
                },
            }],
            store,
        });
        (state, token_secret)
    }

    fn authenticated_request(
        method: &str,
        uri: impl AsRef<str>,
        token: &str,
        body: Body,
    ) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri.as_ref())
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .header(header::CONTENT_TYPE, "application/json")
            .body(body)
            .unwrap()
    }

    #[test]
    fn startup_reaps_only_owned_upload_body_tempfiles() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let owned = root.join(format!(
            "{UPLOAD_BODY_TEMP_PREFIX}crash{UPLOAD_BODY_TEMP_SUFFIX}"
        ));
        let unrelated = root.join(".unrelated.tmp");
        fs::write(&owned, b"orphan").unwrap();
        fs::write(&unrelated, b"keep").unwrap();

        assert_eq!(reap_upload_body_tempfiles(&root).unwrap(), 1);
        assert!(!owned.exists());
        assert_eq!(fs::read(unrelated).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn code_source_http_routes_ingest_a_manifest_without_leaking_store_errors() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-repo", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state.clone());
        let entries = vec![ManifestEntry {
            relative_path: "src/lib.rs".into(),
            content_sha256: "b".repeat(64),
            size: 1,
        }];
        let head = "c".repeat(40);
        let descriptor = GenerationDescriptor {
            schema_version: SCHEMA_VERSION,
            walker_policy_version: WALKER_POLICY_VERSION.into(),
            scope,
            head_commit: head.clone(),
            dirty_fingerprint: dirty_fingerprint(&head, &entries),
            manifest_sha256: manifest_sha256(&entries),
            file_count: 1,
            logical_bytes: 1,
        };
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(
                    serde_json::to_vec(&BeginUploadRequest {
                        descriptor: descriptor.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let begun: BeginUploadResponse = serde_json::from_slice(&body).unwrap();

        let other_token_secret = "e".repeat(64);
        *state.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            enabled: true,
            auth: vec![
                AuthEntry {
                    token: ServiceToken::parse(token.clone()).unwrap(),
                    grant: ProducerGrant {
                        producer_id: "http-test-producer".into(),
                        projects: BTreeMap::from([(
                            descriptor.scope.clone(),
                            "http-test-project".into(),
                        )]),
                    },
                },
                AuthEntry {
                    token: ServiceToken::parse(other_token_secret.clone()).unwrap(),
                    grant: ProducerGrant {
                        producer_id: "other-http-producer".into(),
                        projects: BTreeMap::from([(
                            descriptor.scope.clone(),
                            "http-test-project".into(),
                        )]),
                    },
                },
            ],
            store: state.code_sources.store(),
        });
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                format!(
                    "/internal/code-source/v1/uploads/{}/missing",
                    begun.upload_id
                ),
                &other_token_secret,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "PUT",
                format!(
                    "/internal/code-source/v1/uploads/{}/manifest/0",
                    begun.upload_id
                ),
                &token,
                Body::from(
                    serde_json::to_vec(&ManifestPage {
                        entries: entries.clone(),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                format!(
                    "/internal/code-source/v1/uploads/{}/manifest/complete",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let missing: MissingBlobsPage = serde_json::from_slice(&body).unwrap();
        assert_eq!(missing.hashes, vec!["b".repeat(64)]);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                format!(
                    "/internal/code-source/v1/uploads/{}/finalize",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_upload_state");

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                format!(
                    "/internal/code-source/v1/uploads/{}/missing?cursor=stale",
                    begun.upload_id
                ),
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_code_source_input");

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(format!(
                        "/internal/code-source/v1/uploads/{}/blobs/{}",
                        begun.upload_id,
                        "b".repeat(64)
                    ))
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_LENGTH, "1")
                    .body(Body::from("x"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "invalid_code_source_input");

        let durable =
            anyhow::anyhow!("reading /private/customer/repository/code-sources/secret.json failed");
        let response = HttpError::from_store(durable).into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "storage_unavailable");
        assert!(!error.message.contains("customer"));
        assert!(!error.message.contains('/'));
    }

    #[tokio::test]
    async fn code_source_http_route_uses_typed_contract_errors() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-contract", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state);
        let request = BeginUploadRequest {
            descriptor: GenerationDescriptor {
                schema_version: SCHEMA_VERSION + 1,
                walker_policy_version: WALKER_POLICY_VERSION.into(),
                scope,
                head_commit: "c".repeat(40),
                dirty_fingerprint: "d".repeat(64),
                manifest_sha256: manifest_sha256(&[]),
                file_count: 0,
                logical_bytes: 0,
            },
        };

        let response = app
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&request).unwrap()),
            ))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "unsupported_contract");
    }

    #[tokio::test]
    async fn code_source_http_routes_preserve_auth_not_found_and_store_limit_semantics() {
        let directory = tempfile::tempdir().unwrap();
        let scope = PublishedScope::try_new("http-limits", ".").unwrap();
        let (state, token) = enabled_http_state(directory.path(), &scope);
        let app = router(state.clone()).with_state(state.clone());
        let descriptor = empty_generation_descriptor(scope.clone(), &"c".repeat(40));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/internal/code-source/v1/uploads")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&BeginUploadRequest {
                            descriptor: descriptor.clone(),
                        })
                        .unwrap(),
                    ))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);

        let forbidden = BeginUploadRequest {
            descriptor: empty_generation_descriptor(
                PublishedScope::try_new("other-http-repo", ".").unwrap(),
                &"d".repeat(40),
            ),
        };
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(serde_json::to_vec(&forbidden).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let response = app
            .clone()
            .oneshot(authenticated_request(
                "GET",
                "/internal/code-source/v1/uploads/00000000-0000-4000-8000-000000000000/missing",
                &token,
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);

        let mut limits = StoreLimits::default();
        limits.max_manifest_files = 0;
        state
            .code_sources
            .store()
            .update_limits(limits.clone())
            .unwrap();
        let mut oversized = descriptor.clone();
        oversized.file_count = 1;
        let response = app
            .clone()
            .oneshot(authenticated_request(
                "POST",
                "/internal/code-source/v1/uploads",
                &token,
                Body::from(
                    serde_json::to_vec(&BeginUploadRequest {
                        descriptor: oversized,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
        let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "limit_exceeded");

        limits.max_manifest_files = StoreLimits::default().max_manifest_files;
        limits.max_open_uploads_per_producer = 1;
        state.code_sources.store().update_limits(limits).unwrap();
        for expected in [StatusCode::CREATED, StatusCode::TOO_MANY_REQUESTS] {
            let response = app
                .clone()
                .oneshot(authenticated_request(
                    "POST",
                    "/internal/code-source/v1/uploads",
                    &token,
                    Body::from(
                        serde_json::to_vec(&BeginUploadRequest {
                            descriptor: descriptor.clone(),
                        })
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), expected);
            if expected == StatusCode::TOO_MANY_REQUESTS {
                let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
                let error: ErrorResponse = serde_json::from_slice(&body).unwrap();
                assert_eq!(error.code, "upload_limit_reached");
            }
        }
    }

    #[test]
    fn selector_retirement_retry_budget_is_bounded_and_exponential() {
        let mut attempts = 0;
        let mut delay = std::time::Duration::from_secs(1);
        let mut observed = Vec::new();
        while let Some(next) = take_selector_retirement_retry(&mut attempts, &mut delay) {
            observed.push(next);
        }
        assert_eq!(attempts, SELECTOR_RETIREMENT_RETRY_LIMIT);
        assert_eq!(observed.len(), SELECTOR_RETIREMENT_RETRY_LIMIT as usize);
        assert_eq!(observed.first(), Some(&std::time::Duration::from_secs(1)));
        assert_eq!(observed.last(), Some(&std::time::Duration::from_secs(30)));
    }

    #[test]
    fn queued_retirement_blocks_same_selector_restaging() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = SharedState::for_test(&root);
        let store = state.code_sources.store();
        let selector = bbox_code_source::local_selector("project-a");
        let retirement = RetirementRecord {
            version: 1,
            project_id: "project-a".into(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "a".repeat(32)),
            generation_id: None,
        };
        store.enqueue_retirement(&retirement).unwrap();

        assert!(ensure_selector_staging_available(store.as_ref(), &selector).is_err());

        store.complete_retirement(&retirement).unwrap();
        ensure_selector_staging_available(store.as_ref(), &selector).unwrap();
    }

    #[test]
    fn cold_open_fails_closed_for_every_invalid_enabled_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", root.join("state"));
        let config = crate::config::load().unwrap();
        let store =
            Arc::new(CodeSourceStore::open(root.join("store"), StoreLimits::default()).unwrap());
        let scope_a = PublishedScope::try_new("repo-a", ".").unwrap();
        let scope_b = PublishedScope::try_new("repo-b", ".").unwrap();
        let scope_unknown = PublishedScope::try_new("repo-unknown", ".").unwrap();
        let (project_a, candidate_a) = snapshot_project(&root, "project-a", &scope_a);
        let (project_b, candidate_b) = snapshot_project(&root, "project-b", &scope_b);
        let token_a = root.join("token-a");
        let token_b = root.join("token-b");
        write_service_token(&token_a, 'a');
        write_service_token(&token_b, 'b');
        let producer =
            |producer_id: &str, token_file: &Path, scopes| CodeCollectionProducerConfig {
                producer_id: producer_id.to_string(),
                token_file: token_file.to_path_buf(),
                scopes,
            };

        let broker = snapshot_broker(Vec::new());
        assert_snapshot_rejected(
            &config,
            Vec::new(),
            &[],
            store.clone(),
            &broker,
            "enabled code collection requires at least one producer",
        );

        let mut zero_limits = config.clone();
        zero_limits.code_collection.max_manifest_files = 0;
        assert_snapshot_rejected(
            &zero_limits,
            Vec::new(),
            &[],
            store.clone(),
            &broker,
            "code-collection limits and stale warning hours must be nonzero",
        );

        let broker = snapshot_broker(vec![candidate_a.clone(), candidate_b.clone()]);
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-a", &token_b, vec![scope_b.clone()]),
            ],
            &[project_a.clone(), project_b.clone()],
            store.clone(),
            &broker,
            "duplicate code-collection producer id",
        );
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-b", &token_a, vec![scope_b.clone()]),
            ],
            &[project_a.clone(), project_b.clone()],
            store.clone(),
            &broker,
            "code-collection token values must be unique",
        );
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, Vec::new())],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "enabled code-collection producer has no scopes",
        );
        assert_snapshot_rejected(
            &config,
            vec![
                producer("producer-a", &token_a, vec![scope_a.clone()]),
                producer("producer-b", &token_b, vec![scope_a.clone()]),
            ],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "code-collection scope is assigned more than once",
        );
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, vec![scope_unknown])],
            &[project_a.clone()],
            store.clone(),
            &broker,
            "code-collection scope is not registered",
        );

        let (_, duplicate_candidate) = snapshot_project(&root, "project-b", &scope_a);
        let duplicate_broker = snapshot_broker(vec![candidate_a, duplicate_candidate]);
        assert_snapshot_rejected(
            &config,
            vec![producer("producer-a", &token_a, vec![scope_a])],
            &[project_a, project_b],
            store,
            &duplicate_broker,
            "code-collection scope resolves to multiple registered projects",
        );
    }

    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "current_thread")]
    async fn collected_activation_restart_and_local_cutback_preserve_read_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state_dir = root.join("state");
        let repo = root.join("repo");
        let home = root.join("home");
        fs::create_dir_all(&state_dir).unwrap();
        fs::create_dir_all(repo.join("src")).unwrap();
        fs::create_dir_all(&home).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        git(&repo, &["config", "user.email", "test@example.invalid"]);
        git(&repo, &["config", "user.name", "Blackbox Test"]);
        fs::write(repo.join("src/lib.rs"), "pub fn phase_one() {}\n").unwrap();
        git(&repo, &["add", "src/lib.rs"]);
        git(&repo, &["commit", "-q", "-m", "seed"]);
        let recorded = crate::config::ensure_recorded_repo_id(&repo).unwrap();
        git(&repo, &["add", ".bbox"]);
        git(&repo, &["commit", "-q", "-m", "record repository identity"]);

        let mut env = crate::util::TestEnvGuard::new();
        env.set("HOME", &home);
        env.set("BLACKBOX_CONFIG", root.join("missing-config.toml"));
        env.set("BLACKBOX_STATE_DIR", &state_dir);

        let state = Arc::new(SharedState::for_test(&state_dir));
        let project = state.projects.write().register_path(&repo).unwrap();
        state.persist_projects_durable().await.unwrap();
        let scope = PublishedScope::try_new(recorded.repo_id, ".").unwrap();
        let producer_id = "phase1-transition-producer";
        install_test_assignment(&state, producer_id, &scope, &project.project_id);

        let store = state.code_sources.store();
        let descriptor = empty_generation_descriptor(scope.clone(), &"c".repeat(40));
        let upload = store.begin_upload(producer_id, descriptor).unwrap();
        store
            .complete_manifest(producer_id, &upload.upload_id)
            .unwrap();
        let ready = store
            .finalize_upload(producer_id, &upload.upload_id)
            .unwrap();
        activate_desired_loop(&state, &scope, &project.project_id).unwrap();
        state.index_writer.flush_blocking().unwrap();

        let collected_selector = crate::index::project_files::collected_materialization_selector(
            &project.project_id,
            &ready.generation_id,
        );
        assert_eq!(
            state
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&collected_selector)
        );
        assert_eq!(
            store
                .load_activation(&project.project_id)
                .unwrap()
                .as_ref()
                .map(|activation| activation.generation_id.as_str()),
            Some(ready.generation_id.as_str())
        );

        drop(store);
        drop(state);
        std::thread::sleep(std::time::Duration::from_millis(200));

        let restarted = Arc::new(SharedState::for_test(&state_dir));
        install_test_assignment(&restarted, producer_id, &scope, &project.project_id);
        assert_eq!(
            restarted
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&collected_selector),
            "startup must rebuild read authority from the durable manifest"
        );
        activate_desired_loop(&restarted, &scope, &project.project_id).unwrap();

        let store = restarted.code_sources.store();
        *restarted.code_sources.snapshot.write() = Arc::new(CodeSourceSnapshot {
            enabled: false,
            auth: Vec::new(),
            store: store.clone(),
        });
        cutback_to_local(&restarted, &scope, &project.project_id).unwrap();
        restarted.index_writer.flush_blocking().unwrap();

        let local_selector = bbox_code_source::local_selector(&project.project_id);
        assert_eq!(
            restarted
                .code_read_view
                .read()
                .active_selectors
                .get(&project.project_id),
            Some(&local_selector)
        );
        assert!(
            store
                .load_activation(&project.project_id)
                .unwrap()
                .is_none()
        );
        let edges_dir = crate::edge_index::edges_dir_from_bro_store(&restarted.store_dir);
        let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir).unwrap();
        assert_eq!(
            manifest
                .workspaces
                .get(&project.project_id)
                .and_then(|entry| entry.code_source_selector.as_deref()),
            Some(local_selector.as_str())
        );

        for _ in 0..500 {
            if !store.retirement_pending(&collected_selector).unwrap() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!store.retirement_pending(&collected_selector).unwrap());
    }

    #[test]
    fn startup_recovery_distinguishes_exact_and_selectorless_collision_work() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("code-source");
        let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let project_id = ProjectId::parse("startup-collision").unwrap();
        let scope = PublishedScope::try_new("startup-repo", ".").unwrap();
        let exact_descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
        let retained_descriptor = empty_generation_descriptor(scope.clone(), &"b".repeat(40));
        let exact_generation_id = compute_generation_id("startup-exact-host", &exact_descriptor);
        let retained_generation_id =
            compute_generation_id("startup-retained-host", &retained_descriptor);
        for (generation_id, producer_id, descriptor) in [
            (
                &exact_generation_id,
                "startup-exact-host",
                exact_descriptor.clone(),
            ),
            (
                &retained_generation_id,
                "startup-retained-host",
                retained_descriptor.clone(),
            ),
        ] {
            let metadata_path = paths.generation_metadata(&scope, generation_id).unwrap();
            fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
            fs::write(
                metadata_path,
                encode_stored_generation_v2_for_migration(&StoredGenerationV2 {
                    version: 2,
                    generation_id: generation_id.clone(),
                    producer_id: producer_id.to_string(),
                    ordinal: 1,
                    descriptor,
                    published_scope: scope.clone(),
                    state: GenerationState::Ready,
                    diagnostic: None,
                    created_unix_secs: 1,
                    materialized_doc_count: None,
                    entity_inventory_sha256: None,
                })
                .unwrap(),
            )
            .unwrap();
        }
        let exact_selector = format!(
            "{}:m0123456789abcdef",
            source_selector(project_id.as_str(), &exact_generation_id)
        );
        let lifecycle = CollisionRetirementLifecycleV1 {
            version: 1,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    exact_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                            exact_selector.clone(),
                        ),
                        snapshot_id: format!("collected-{}", "c".repeat(32)),
                        manifest_sha256: exact_descriptor.manifest_sha256,
                        inventory_hash: "e".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
                (
                    retained_generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope,
                        selector_evidence: CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "c".repeat(32)),
                        manifest_sha256: retained_descriptor.manifest_sha256,
                        inventory_hash: "2".repeat(64),
                        plan_hash: "f".repeat(64),
                    },
                ),
            ]),
        };
        let lifecycle_path = paths.collision_retirement_pending(&project_id);
        fs::create_dir_all(lifecycle_path.parent().unwrap()).unwrap();
        fs::write(
            &lifecycle_path,
            encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
        )
        .unwrap();

        let first_recovery = collision_retirement_tasks_for_recovery(&store).unwrap();

        assert_eq!(first_recovery.len(), 2);
        assert!(first_recovery.iter().any(|task| matches!(
            task,
            CollisionRetirementRecoveryTask::Exact { work, selector }
                if work.generation_id == exact_generation_id && selector == &exact_selector
        )));
        assert!(first_recovery.iter().any(|task| matches!(
            task,
            CollisionRetirementRecoveryTask::Selectorless { work }
                if work.generation_id == retained_generation_id
                    && work.exact_selector().is_none()
        )));
        let queued =
            decode_collision_retirement_pending_for_migration(&fs::read(&lifecycle_path).unwrap())
                .unwrap();
        assert!(
            queued
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Queued)
        );

        store
            .repair_and_complete_collision_retirement(&project_id, &retained_generation_id)
            .unwrap();
        let restarted = collision_retirement_tasks_for_recovery(&store).unwrap();
        assert_eq!(restarted.len(), 1);
        assert!(matches!(
            &restarted[0],
            CollisionRetirementRecoveryTask::Exact { work, selector }
                if work.generation_id == exact_generation_id && selector == &exact_selector
        ));

        store
            .repair_and_complete_collision_retirement(&project_id, &exact_generation_id)
            .unwrap();
        assert!(
            collision_retirement_tasks_for_recovery(&store)
                .unwrap()
                .is_empty()
        );
        let completed =
            decode_collision_retirement_pending_for_migration(&fs::read(&lifecycle_path).unwrap())
                .unwrap();
        assert!(
            completed
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Completed)
        );
    }

    #[test]
    fn startup_recovery_refuses_orphan_exact_work_before_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("code-source");
        let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
        let paths = CodeSourceStorePaths::new(root).unwrap();
        let project_id = ProjectId::parse("orphan-collision").unwrap();
        let generation_id = "a".repeat(64);
        let work = CollisionRetirementWorkV1 {
            version: 1,
            project_id: project_id.clone(),
            generation_id: generation_id.clone(),
            former_scope: PublishedScope::try_new("orphan-repo", ".").unwrap(),
            selector_evidence: CollisionRetirementSelectorEvidenceV1::ExactMaterialized(format!(
                "{}:m0123456789abcdef",
                source_selector(project_id.as_str(), &generation_id)
            )),
            snapshot_id: format!("collected-{}", "b".repeat(32)),
            manifest_sha256: "c".repeat(64),
            inventory_hash: "d".repeat(64),
            plan_hash: "e".repeat(64),
        };
        let work_path = paths
            .collision_retirement_work(&project_id, &generation_id)
            .unwrap();
        fs::write(work_path, serde_json::to_vec_pretty(&work).unwrap()).unwrap();

        let error = collision_retirement_tasks_for_recovery(&store).unwrap_err();

        assert!(error.to_string().contains("orphaned"));
    }

    #[test]
    fn collision_terminal_transition_failure_preserves_work_and_retries_for_both_targets() {
        for (project_name, producer_id, exact_selector) in [
            ("repair-exact", "repair-host-exact", true),
            ("repair-retained", "repair-host-retained", false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap().join("code-source");
            let store = CodeSourceStore::open(&root, StoreLimits::default()).unwrap();
            let paths = CodeSourceStorePaths::new(root).unwrap();
            let project_id = ProjectId::parse(project_name).unwrap();
            let scope = PublishedScope::try_new(format!("{project_name}-repo"), ".").unwrap();
            let descriptor = empty_generation_descriptor(scope.clone(), &"a".repeat(40));
            let generation_id = compute_generation_id(producer_id, &descriptor);
            let selector_evidence = if exact_selector {
                CollisionRetirementSelectorEvidenceV1::ExactMaterialized(format!(
                    "{}:m0123456789abcdef",
                    source_selector(project_id.as_str(), &generation_id)
                ))
            } else {
                CollisionRetirementSelectorEvidenceV1::NoDurableSelector
            };
            let lifecycle = CollisionRetirementLifecycleV1 {
                version: 1,
                project_id: project_id.clone(),
                entries: BTreeMap::from([(
                    generation_id.clone(),
                    CollisionRetirementEntryV1 {
                        state: CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: scope.clone(),
                        selector_evidence,
                        snapshot_id: format!("collected-{}", "b".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "c".repeat(64),
                        plan_hash: "d".repeat(64),
                    },
                )]),
            };
            let lifecycle_path = paths.collision_retirement_pending(&project_id);
            fs::write(
                &lifecycle_path,
                encode_collision_retirement_pending_for_migration(&lifecycle).unwrap(),
            )
            .unwrap();
            let tasks = collision_retirement_tasks_for_recovery(&store).unwrap();
            assert_eq!(tasks.len(), 1);
            assert_eq!(
                matches!(tasks[0], CollisionRetirementRecoveryTask::Exact { .. }),
                exact_selector
            );

            assert!(
                repair_and_complete_collision_retirement(&store, &project_id, &generation_id)
                    .is_err()
            );
            let queued = decode_collision_retirement_pending_for_migration(
                &fs::read(&lifecycle_path).unwrap(),
            )
            .unwrap();
            assert_eq!(
                queued.entry(&generation_id).unwrap().state,
                CollisionRetirementLifecycleStateV1::Queued
            );
            assert_eq!(store.collision_retirement_work_records().unwrap().len(), 1);

            let stored = StoredGenerationV2 {
                version: 2,
                generation_id: generation_id.clone(),
                producer_id: producer_id.to_string(),
                ordinal: 1,
                descriptor,
                published_scope: scope.clone(),
                state: GenerationState::Ready,
                diagnostic: Some("stale collision state".into()),
                created_unix_secs: 1,
                materialized_doc_count: None,
                entity_inventory_sha256: None,
            };
            let metadata_path = paths.generation_metadata(&scope, &generation_id).unwrap();
            fs::create_dir_all(metadata_path.parent().unwrap()).unwrap();
            fs::write(
                &metadata_path,
                encode_stored_generation_v2_for_migration(&stored).unwrap(),
            )
            .unwrap();
            repair_and_complete_collision_retirement(&store, &project_id, &generation_id).unwrap();

            assert_eq!(
                decode_stored_generation_v2_for_migration(&fs::read(metadata_path).unwrap())
                    .unwrap()
                    .state,
                GenerationState::Superseded
            );
            let completed = decode_collision_retirement_pending_for_migration(
                &fs::read(&lifecycle_path).unwrap(),
            )
            .unwrap();
            assert_eq!(
                completed.entry(&generation_id).unwrap().state,
                CollisionRetirementLifecycleStateV1::Completed
            );
            assert!(
                store
                    .collision_retirement_work_records()
                    .unwrap()
                    .is_empty()
            );
        }
    }
}

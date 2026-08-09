//! Authenticated knowledge and gap source intake.
//!
//! Publication candidates use the existing project-scoped producer grant.
//! Provisional snapshots use an exact, expiring workspace binding. Both
//! middleware boundaries run before Axum parses a request body.

use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Result, bail};
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use bbox_code_source::ErrorResponse;
use bbox_knowledge_source::{
    AncestryPageV1, BeginProvisionalUploadRequestV1, BeginPublicationUploadRequestV1,
    BeginSourceUploadResponseV1, ContractError, FinalizeProvisionalUploadRequestV1,
    FinalizeSourceUploadResponseV1, KnowledgeSourceLimits, MAX_ANCESTRY_PAGE_BYTES,
    MAX_MANIFEST_PAGE_BYTES, MAX_SOURCE_FILE_BYTES, MissingSourceBlobsPageV1,
    ProvisionalProbeRequestV1, ProvisionalProbeResponseV1, ProvisionalWorkspaceStatusV1,
    PublicationCandidateStatusV1, PublicationProbeRequestV1, PublicationProbeResponseV1,
    RenewProvisionalGenerationRequestV1, SnapshotClassV1, SourceLaneV1, SourceManifestPageV1,
};
use bbox_knowledge_source_store::{
    KnowledgeSourceStore, ProvisionalAuthorityV1, PublicationAuthorityV1, StoreLimits,
    StoreRequestError,
};
use bro_core::WorkspaceId;
use bro_rpc::ServiceToken;
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};

use super::SharedState;
use super::producer_auth::{ProducerGrant, RepoTransportGrantError};

const UPLOAD_BODY_TEMP_PREFIX: &str = ".knowledge-source-upload-body-";
const UPLOAD_BODY_TEMP_SUFFIX: &str = ".tmp";

#[derive(Clone)]
pub(crate) struct WorkspaceBindingGrant {
    pub(crate) project_id: String,
    pub(crate) scope: bbox_corpus_core::identity::PublishedScope,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) expires_unix_secs: u64,
}

struct WorkspaceBindingEntry {
    token: ServiceToken,
    grant: WorkspaceBindingGrant,
}

pub(crate) struct KnowledgeSourceRuntime {
    store: Arc<KnowledgeSourceStore>,
    workspace_bindings: parking_lot::RwLock<Vec<WorkspaceBindingEntry>>,
}

impl KnowledgeSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config) -> Result<Self> {
        let store = Arc::new(KnowledgeSourceStore::open(
            config.paths.state_dir.join("knowledge-sources"),
            checked_store_limits(config)?,
        )?);
        reap_upload_body_tempfiles(store.root())?;
        Ok(Self {
            store,
            workspace_bindings: parking_lot::RwLock::new(Vec::new()),
        })
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        Self {
            store: Arc::new(
                KnowledgeSourceStore::open(root.join("knowledge-sources"), StoreLimits::default())
                    .unwrap(),
            ),
            workspace_bindings: parking_lot::RwLock::new(Vec::new()),
        }
    }

    pub(crate) fn store(&self) -> Arc<KnowledgeSourceStore> {
        self.store.clone()
    }

    pub(crate) fn validate_config(config: &crate::config::Config) -> Result<()> {
        checked_store_limits(config).map(|_| ())
    }

    pub(crate) fn update_limits(&self, config: &crate::config::Config) -> Result<()> {
        self.store.update_limits(checked_store_limits(config)?)
    }

    fn authenticate_workspace_binding(
        &self,
        candidate: &str,
        now: u64,
    ) -> Option<WorkspaceBindingGrant> {
        let mut matched = None;
        for entry in self.workspace_bindings.read().iter() {
            if entry.token.verify(candidate) && entry.grant.expires_unix_secs > now {
                matched = Some(entry.grant.clone());
            }
        }
        matched
    }

    #[cfg(test)]
    pub(crate) fn install_workspace_binding_for_test(
        &self,
        token: ServiceToken,
        grant: WorkspaceBindingGrant,
    ) {
        self.workspace_bindings
            .write()
            .push(WorkspaceBindingEntry { token, grant });
    }
}

fn store_limits(config: &crate::config::Config) -> StoreLimits {
    StoreLimits {
        contract: KnowledgeSourceLimits::default(),
        max_open_uploads_per_authority: config.code_collection.max_open_uploads_per_producer,
        retained_publication_generations: config.code_collection.retained_generations,
        unreferenced_blob_grace_secs: config
            .code_collection
            .unreferenced_blob_grace_hours
            .saturating_mul(60 * 60),
        ..StoreLimits::default()
    }
}

fn checked_store_limits(config: &crate::config::Config) -> Result<StoreLimits> {
    let limits = store_limits(config);
    if limits.max_open_uploads_per_authority == 0
        || limits.retained_publication_generations == 0
        || limits.upload_idle_ttl_secs == 0
        || limits.max_provisional_lease_secs == 0
        || limits.retained_provisional_generations == 0
    {
        bail!("knowledge-source limits must be nonzero");
    }
    Ok(limits)
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    publication_router(state.clone()).merge(provisional_router(state))
}

fn publication_router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/knowledge-source/v1/publication/probe",
            post(probe_publication).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/publication/uploads",
            post(begin_publication_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/publication/uploads/{upload_id}/manifest/{lane}/{page}",
            post(put_publication_manifest_page)
                .layer(DefaultBodyLimit::max(MAX_MANIFEST_PAGE_BYTES as usize)),
        )
        .route(
            "/internal/knowledge-source/v1/publication/uploads/{upload_id}/missing",
            get(missing_publication_blobs),
        )
        .route(
            "/internal/knowledge-source/v1/publication/uploads/{upload_id}/blobs/{hash}",
            put(put_publication_blob).layer(DefaultBodyLimit::max(MAX_SOURCE_FILE_BYTES as usize)),
        )
        .route(
            "/internal/knowledge-source/v1/publication/uploads/{upload_id}/finalize",
            post(finalize_publication_upload).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/knowledge-source/v1/publication/generations/{generation}/status",
            get(publication_generation_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_knowledge_source_request,
        ))
}

fn provisional_router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/knowledge-source/v1/provisional/probe",
            post(probe_provisional).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads",
            post(begin_provisional_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads/{upload_id}/ancestry/{page}",
            post(put_provisional_ancestry_page)
                .layer(DefaultBodyLimit::max(MAX_ANCESTRY_PAGE_BYTES as usize)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads/{upload_id}/manifest/{class}/{lane}/{page}",
            post(put_provisional_manifest_page)
                .layer(DefaultBodyLimit::max(MAX_MANIFEST_PAGE_BYTES as usize)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads/{upload_id}/missing",
            get(missing_provisional_blobs),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads/{upload_id}/blobs/{hash}",
            put(put_provisional_blob)
                .layer(DefaultBodyLimit::max(MAX_SOURCE_FILE_BYTES as usize)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/uploads/{upload_id}/finalize",
            post(finalize_provisional_upload).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/generations/{generation}/renew",
            post(renew_provisional_generation).layer(DefaultBodyLimit::max(16 * 1024)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/generations/{generation}/retire",
            post(retire_provisional_generation).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/knowledge-source/v1/provisional/generations/{generation}/status",
            get(provisional_generation_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            authenticate_workspace_source_request,
        ))
}

async fn probe_publication(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<PublicationProbeRequestV1>,
) -> Result<Json<PublicationProbeResponseV1>, HttpError> {
    request
        .validate()
        .map_err(|error| HttpError::from_contract(&error))?;
    let authority = require_project_grant(&state, &grant, &request.scope)?;
    let store = state.knowledge_sources.store();
    let current = blocking(move || {
        store.probe_publication(
            &authority,
            &request.full_ref,
            &request.publisher_commit,
            request.object_format,
        )
    })
    .await?;
    Ok(Json(PublicationProbeResponseV1 { current }))
}

async fn begin_publication_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Json(request): Json<BeginPublicationUploadRequestV1>,
) -> Result<(StatusCode, Json<BeginSourceUploadResponseV1>), HttpError> {
    let authority = require_project_grant(&state, &grant, &request.descriptor.scope)?;
    let store = state.knowledge_sources.store();
    let response =
        blocking(move || store.begin_publication_upload(&authority, request.descriptor)).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn put_publication_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, lane, page)): Path<(String, String, u64)>,
    Json(page_body): Json<SourceManifestPageV1>,
) -> Result<StatusCode, HttpError> {
    let lane = parse_lane(&lane)?;
    let store = state.knowledge_sources.store();
    let authority = require_publication_upload_grant(&state, &store, &grant, &upload_id).await?;
    blocking(move || {
        store.put_publication_manifest_page(&authority, &upload_id, lane, page, &page_body)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingQuery {
    cursor: Option<String>,
}

async fn missing_publication_blobs(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingSourceBlobsPageV1>, HttpError> {
    let store = state.knowledge_sources.store();
    let authority = require_publication_upload_grant(&state, &store, &grant, &upload_id).await?;
    Ok(Json(
        blocking(move || {
            store.missing_publication_blobs(&authority, &upload_id, query.cursor.as_deref())
        })
        .await?,
    ))
}

async fn put_publication_blob(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let store = state.knowledge_sources.store();
    let authority = require_publication_upload_grant(&state, &store, &grant, &upload_id).await?;
    let expected_size = {
        let store = store.clone();
        let authority = authority.clone();
        let upload_id = upload_id.clone();
        let hash = hash.clone();
        blocking(move || store.expected_publication_blob_size(&authority, &upload_id, &hash))
            .await?
    };
    let file = bounded_body_file(store.root(), &headers, body, expected_size).await?;
    blocking(move || {
        store.install_publication_blob(&authority, &upload_id, &hash, expected_size, file)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize_publication_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(upload_id): Path<String>,
) -> Result<(StatusCode, Json<FinalizeSourceUploadResponseV1>), HttpError> {
    let store = state.knowledge_sources.store();
    let authority = require_publication_upload_grant(&state, &store, &grant, &upload_id).await?;
    let response =
        blocking(move || store.finalize_publication_upload(&authority, &upload_id)).await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn publication_generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ProducerGrant>,
    Path(generation): Path<String>,
) -> Result<Json<PublicationCandidateStatusV1>, HttpError> {
    let store = state.knowledge_sources.store();
    require_publication_generation_grant(&state, &store, &grant, &generation).await?;
    let producer_id = grant.producer_id;
    Ok(Json(
        blocking(move || store.publication_status(&producer_id, &generation)).await?,
    ))
}

async fn probe_provisional(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Json(request): Json<ProvisionalProbeRequestV1>,
) -> Result<Json<ProvisionalProbeResponseV1>, HttpError> {
    request
        .validate()
        .map_err(|error| HttpError::from_contract(&error))?;
    let authority = provisional_authority(&grant);
    if request.scope != authority.scope || request.workspace_id != authority.workspace_id {
        return Err(HttpError::forbidden_scope());
    }
    let store = state.knowledge_sources.store();
    let current = blocking(move || store.probe_provisional(&authority, now_unix_secs())).await?;
    Ok(Json(ProvisionalProbeResponseV1 { current }))
}

async fn begin_provisional_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Json(request): Json<BeginProvisionalUploadRequestV1>,
) -> Result<(StatusCode, Json<BeginSourceUploadResponseV1>), HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    let response =
        blocking(move || store.begin_provisional_upload(&authority, request.descriptor)).await?;
    Ok((StatusCode::CREATED, Json(response)))
}

async fn put_provisional_ancestry_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path((upload_id, page)): Path<(String, u64)>,
    Json(page_body): Json<AncestryPageV1>,
) -> Result<StatusCode, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    blocking(move || store.put_provisional_ancestry_page(&authority, &upload_id, page, &page_body))
        .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn put_provisional_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path((upload_id, class, lane, page)): Path<(String, String, String, u64)>,
    Json(page_body): Json<SourceManifestPageV1>,
) -> Result<StatusCode, HttpError> {
    let class = parse_class(&class)?;
    let lane = parse_lane(&lane)?;
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    blocking(move || {
        store.put_provisional_manifest_page(&authority, &upload_id, class, lane, page, &page_body)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn missing_provisional_blobs(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingSourceBlobsPageV1>, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    Ok(Json(
        blocking(move || {
            store.missing_provisional_blobs(&authority, &upload_id, query.cursor.as_deref())
        })
        .await?,
    ))
}

async fn put_provisional_blob(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    let expected_size = {
        let store = store.clone();
        let authority = authority.clone();
        let upload_id = upload_id.clone();
        let hash = hash.clone();
        blocking(move || store.expected_provisional_blob_size(&authority, &upload_id, &hash))
            .await?
    };
    let file = bounded_body_file(store.root(), &headers, body, expected_size).await?;
    blocking(move || {
        store.install_provisional_blob(&authority, &upload_id, &hash, expected_size, file)
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize_provisional_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path(upload_id): Path<String>,
    Json(request): Json<FinalizeProvisionalUploadRequestV1>,
) -> Result<(StatusCode, Json<FinalizeSourceUploadResponseV1>), HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    let response = blocking(move || {
        store.finalize_provisional_upload(&authority, &upload_id, request.lease_ttl_secs)
    })
    .await?;
    Ok((StatusCode::ACCEPTED, Json(response)))
}

async fn renew_provisional_generation(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path(generation): Path<String>,
    Json(request): Json<RenewProvisionalGenerationRequestV1>,
) -> Result<Json<ProvisionalWorkspaceStatusV1>, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    Ok(Json(
        blocking(move || store.renew_provisional(&authority, &generation, request.lease_ttl_secs))
            .await?,
    ))
}

async fn retire_provisional_generation(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path(generation): Path<String>,
) -> Result<StatusCode, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    blocking(move || store.retire_provisional(&authority, &generation)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn provisional_generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<WorkspaceBindingGrant>,
    Path(generation): Path<String>,
) -> Result<Json<ProvisionalWorkspaceStatusV1>, HttpError> {
    let authority = provisional_authority(&grant);
    let store = state.knowledge_sources.store();
    Ok(Json(
        blocking(move || store.provisional_status(&authority, &generation)).await?,
    ))
}

async fn authenticate_workspace_source_request(
    State(state): State<Arc<SharedState>>,
    mut request: Request,
    next: Next,
) -> Response {
    if !state
        .code_sources
        .producer_auth()
        .knowledge_transport_enabled()
    {
        return error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "knowledge_transport_disabled",
            "knowledge transport is disabled",
        );
    }
    let candidate = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let Some(grant) = candidate.and_then(|candidate| {
        state
            .knowledge_sources
            .authenticate_workspace_binding(candidate, now_unix_secs())
    }) else {
        return error_response(StatusCode::UNAUTHORIZED, "unauthorized", "unauthorized");
    };
    request.extensions_mut().insert(grant);
    next.run(request).await
}

fn require_project_grant(
    state: &SharedState,
    grant: &ProducerGrant,
    scope: &bbox_corpus_core::identity::PublishedScope,
) -> Result<PublicationAuthorityV1, HttpError> {
    let auth = state.code_sources.producer_auth();
    if !auth.knowledge_transport_enabled() {
        return Err(HttpError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "knowledge_transport_disabled",
            "knowledge transport is disabled",
        ));
    }
    let project_id = auth
        .project_transport_grant(grant, scope)
        .map_err(HttpError::from_grant)?;
    Ok(PublicationAuthorityV1 {
        producer_id: grant.producer_id.clone(),
        project_id: project_id.as_str().to_string(),
        scope: scope.clone(),
    })
}

async fn require_publication_upload_grant(
    state: &SharedState,
    store: &Arc<KnowledgeSourceStore>,
    grant: &ProducerGrant,
    upload_id: &str,
) -> Result<PublicationAuthorityV1, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let upload_id = upload_id.to_string();
    let authority =
        blocking(move || store.publication_upload_authority(&producer_id, &upload_id)).await?;
    require_matching_publication_authority(state, grant, authority)
}

async fn require_publication_generation_grant(
    state: &SharedState,
    store: &Arc<KnowledgeSourceStore>,
    grant: &ProducerGrant,
    generation: &str,
) -> Result<PublicationAuthorityV1, HttpError> {
    let store = store.clone();
    let producer_id = grant.producer_id.clone();
    let generation = generation.to_string();
    let authority =
        blocking(move || store.publication_generation_authority(&producer_id, &generation)).await?;
    require_matching_publication_authority(state, grant, authority)
}

fn require_matching_publication_authority(
    state: &SharedState,
    grant: &ProducerGrant,
    authority: PublicationAuthorityV1,
) -> Result<PublicationAuthorityV1, HttpError> {
    let current = require_project_grant(state, grant, &authority.scope)?;
    if current.project_id != authority.project_id || current.producer_id != authority.producer_id {
        return Err(HttpError::new(
            StatusCode::CONFLICT,
            "knowledge_source_authority_changed",
            "knowledge-source project authority changed",
        ));
    }
    Ok(authority)
}

fn provisional_authority(grant: &WorkspaceBindingGrant) -> ProvisionalAuthorityV1 {
    ProvisionalAuthorityV1 {
        project_id: grant.project_id.clone(),
        scope: grant.scope.clone(),
        workspace_id: grant.workspace_id.clone(),
    }
}

fn parse_lane(value: &str) -> Result<SourceLaneV1, HttpError> {
    match value {
        "knowledge" => Ok(SourceLaneV1::Knowledge),
        "gaps" => Ok(SourceLaneV1::Gaps),
        _ => Err(HttpError::unprocessable(
            "knowledge_source_manifest_invalid",
            "knowledge-source lane is invalid",
        )),
    }
}

fn parse_class(value: &str) -> Result<SnapshotClassV1, HttpError> {
    match value {
        "baseline" => Ok(SnapshotClassV1::Baseline),
        "working" => Ok(SnapshotClassV1::Working),
        _ => Err(HttpError::unprocessable(
            "knowledge_source_manifest_invalid",
            "knowledge-source snapshot class is invalid",
        )),
    }
}

async fn bounded_body_file(
    store_root: &std::path::Path,
    headers: &HeaderMap,
    body: Body,
    expected_size: u64,
) -> Result<std::fs::File, HttpError> {
    let content_length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| {
            HttpError::unprocessable("content_length_required", "exact Content-Length required")
        })?;
    if content_length != expected_size {
        return Err(HttpError::unprocessable(
            "knowledge_source_blob_mismatch",
            "Content-Length does not match the manifest",
        ));
    }
    let temporary = tempfile::Builder::new()
        .prefix(UPLOAD_BODY_TEMP_PREFIX)
        .suffix(UPLOAD_BODY_TEMP_SUFFIX)
        .tempfile_in(store_root)
        .map_err(HttpError::storage)?;
    let mut file = tokio::fs::File::from_std(temporary.reopen().map_err(HttpError::storage)?);
    let mut stream = body.into_data_stream();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk =
            chunk.map_err(|error| HttpError::unprocessable("invalid_body", error.to_string()))?;
        written = written.checked_add(chunk.len() as u64).ok_or_else(|| {
            HttpError::too_large(
                "knowledge_source_blob_mismatch",
                "knowledge-source blob is too large",
            )
        })?;
        if written > expected_size {
            return Err(HttpError::too_large(
                "knowledge_source_blob_mismatch",
                "knowledge-source blob exceeds its manifest size",
            ));
        }
        file.write_all(&chunk).await.map_err(HttpError::storage)?;
    }
    if written != expected_size {
        return Err(HttpError::unprocessable(
            "knowledge_source_blob_mismatch",
            "knowledge-source blob is shorter than its manifest size",
        ));
    }
    file.sync_all().await.map_err(HttpError::storage)?;
    file.seek(SeekFrom::Start(0))
        .await
        .map_err(HttpError::storage)?;
    Ok(file.into_std().await)
}

async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::storage("knowledge-source blocking task failed"))?
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

fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ErrorResponse {
            code: code.to_string(),
            message: message.to_string(),
        }),
    )
        .into_response()
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

    fn forbidden_scope() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "knowledge_source_scope_forbidden",
            "knowledge-source scope is not authorized",
        )
    }

    fn from_grant(error: RepoTransportGrantError) -> Self {
        match error {
            RepoTransportGrantError::ScopeForbidden => Self::forbidden_scope(),
            RepoTransportGrantError::RepoHistoryNotFound
            | RepoTransportGrantError::RepoHistoryScopeSplit => Self::new(
                StatusCode::CONFLICT,
                "knowledge_source_scope_forbidden",
                "knowledge-source project authority is unavailable",
            ),
        }
    }

    fn from_contract(error: &ContractError) -> Self {
        match error {
            ContractError::ManifestLimitExceeded
            | ContractError::AncestryLimitExceeded
            | ContractError::GenerationLimitExceeded
            | ContractError::InvalidLimit => Self::too_large(
                "knowledge_source_limit_exceeded",
                "knowledge-source input exceeds an enforced limit",
            ),
            ContractError::UnsupportedSchema(_) => Self::unprocessable(
                "knowledge_source_contract_unsupported",
                "knowledge-source contract version is unsupported",
            ),
            ContractError::AncestryIncomplete
            | ContractError::AncestryUnreachable
            | ContractError::AncestryCycle => Self::unprocessable(
                "knowledge_source_ancestry_incomplete",
                "knowledge-source ancestry evidence is invalid",
            ),
            ContractError::InvalidMergeBase => Self::unprocessable(
                "knowledge_source_merge_base_mismatch",
                "knowledge-source merge base is invalid",
            ),
            ContractError::ManifestCommitmentMismatch
            | ContractError::ManifestCountMismatch
            | ContractError::ManifestOutOfOrder
            | ContractError::InvalidManifestPage
            | ContractError::InvalidSourceFilename => Self::unprocessable(
                "knowledge_source_manifest_invalid",
                "knowledge-source manifest is invalid",
            ),
            _ => Self::unprocessable(
                "knowledge_source_input_invalid",
                "knowledge-source input violates the transport contract",
            ),
        }
    }

    fn storage(error: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %error, "knowledge-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "knowledge_source_storage_unavailable",
            "knowledge-source storage is unavailable",
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
                    "knowledge_source_limit_exceeded",
                    "knowledge-source input exceeds an enforced limit",
                ),
                StoreRequestError::TooManyOpenUploads => Self::new(
                    StatusCode::TOO_MANY_REQUESTS,
                    "knowledge_source_upload_limit_reached",
                    "authority has too many open knowledge-source uploads",
                ),
                StoreRequestError::InvalidState => Self::new(
                    StatusCode::CONFLICT,
                    "knowledge_source_state_conflict",
                    "knowledge-source resource is not in the required state",
                ),
                StoreRequestError::InvalidInput => Self::unprocessable(
                    "knowledge_source_input_invalid",
                    "knowledge-source input is invalid",
                ),
                StoreRequestError::Conflict => Self::new(
                    StatusCode::CONFLICT,
                    "knowledge_source_generation_conflict",
                    "knowledge-source evidence conflicts with durable state",
                ),
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

    use axum::body::to_bytes;
    use axum::http::Request;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, CommitNamespace, CorpusProject, ProjectId, ProjectScope,
        RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization,
        RepoHistoryRecord,
    };
    use bbox_knowledge_source::{
        AncestryCommitV1, AncestryDescriptorV1, GitObjectFormatV1, SCHEMA_VERSION,
        SourceFileManifestEntryV1, SourceManifestDescriptorV1, StableCaptureV1, ancestry_sha256,
        source_file_blob_sha256, source_manifest_sha256, working_pair_sha256,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::server::producer_auth::ProducerAuthRuntime;

    const KNOWLEDGE_BYTES: &[u8] = br#"{"id":"knowledge-1"}"#;

    struct TestAuthority {
        state: Arc<SharedState>,
        producer_token: String,
        other_producer_token: String,
        workspace_token: String,
        other_workspace_token: String,
        expired_workspace_token: String,
        scope: PublishedScope,
        workspace_id: WorkspaceId,
    }

    fn project(
        project_id: &str,
        scope: PublishedScope,
        repo_history_id: RepoHistoryId,
    ) -> CorpusProject {
        CorpusProject {
            project_id: ProjectId::parse(project_id).unwrap(),
            scope: ProjectScope::Published(scope),
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: "knowledge transport fixture".to_string(),
            created_at: "2026-08-08T00:00:00Z".to_string(),
            registered_at_compat: None,
            repo_history: Some(repo_history_id),
            languages: BTreeSet::new(),
        }
    }

    fn repo_history(id: &str, authority: &str) -> (RepoHistoryId, RepoHistoryRecord) {
        let repo_history_id = RepoHistoryId::parse(id).unwrap();
        (
            repo_history_id.clone(),
            RepoHistoryRecord {
                repo_history_id,
                membership_generation: 0,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse(authority).unwrap(),
                ),
                primary_namespace: CommitNamespace::parse(authority).unwrap(),
                compatibility_namespaces: BTreeSet::new(),
                materialization: RepoHistoryMaterialization::NotBuilt,
            },
        )
    }

    fn enabled_state(root: &std::path::Path) -> TestAuthority {
        let state = Arc::new(SharedState::for_test(root));
        let scope = PublishedScope::try_new("knowledge-http-repo", ".").unwrap();
        let other_scope = PublishedScope::try_new("other-knowledge-http-repo", ".").unwrap();
        let (history_id, history) =
            repo_history("rh_00000000000000000000000000000001", "knowledge-http-repo");
        let (other_history_id, other_history) = repo_history(
            "rh_00000000000000000000000000000002",
            "other-knowledge-http-repo",
        );
        let project_id = ProjectId::parse("p_00000000000000000000000000000001").unwrap();
        let other_project_id = ProjectId::parse("p_00000000000000000000000000000002").unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.repo_histories.insert(history_id.clone(), history);
        catalog
            .repo_histories
            .insert(other_history_id.clone(), other_history);
        catalog.projects.insert(
            project_id.clone(),
            project(project_id.as_str(), scope.clone(), history_id),
        );
        catalog.projects.insert(
            other_project_id.clone(),
            project(
                other_project_id.as_str(),
                other_scope.clone(),
                other_history_id,
            ),
        );
        catalog.validate().unwrap();

        let producer_token = "1".repeat(64);
        let other_producer_token = "2".repeat(64);
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![
                    (
                        ServiceToken::parse(producer_token.clone()).unwrap(),
                        ProducerGrant {
                            producer_id: "knowledge-producer-a".to_string(),
                            projects: BTreeMap::from([(
                                scope.clone(),
                                project_id.as_str().to_string(),
                            )]),
                        },
                    ),
                    (
                        ServiceToken::parse(other_producer_token.clone()).unwrap(),
                        ProducerGrant {
                            producer_id: "knowledge-producer-b".to_string(),
                            projects: BTreeMap::from([(
                                other_scope,
                                other_project_id.as_str().to_string(),
                            )]),
                        },
                    ),
                ],
                &catalog,
            )));

        let workspace_id = WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let other_workspace_id = WorkspaceId::parse("fedcba9876543210fedcba9876543210").unwrap();
        let workspace_token = "3".repeat(64);
        let other_workspace_token = "4".repeat(64);
        let expired_workspace_token = "5".repeat(64);
        for (token, bound_workspace, expires) in [
            (
                workspace_token.clone(),
                workspace_id.clone(),
                now_unix_secs() + 600,
            ),
            (
                other_workspace_token.clone(),
                other_workspace_id,
                now_unix_secs() + 600,
            ),
            (expired_workspace_token.clone(), workspace_id.clone(), 1),
        ] {
            state.knowledge_sources.install_workspace_binding_for_test(
                ServiceToken::parse(token).unwrap(),
                WorkspaceBindingGrant {
                    project_id: project_id.as_str().to_string(),
                    scope: scope.clone(),
                    workspace_id: bound_workspace,
                    expires_unix_secs: expires,
                },
            );
        }
        TestAuthority {
            state,
            producer_token,
            other_producer_token,
            workspace_token,
            other_workspace_token,
            expired_workspace_token,
            scope,
            workspace_id,
        }
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

    fn entry() -> SourceFileManifestEntryV1 {
        SourceFileManifestEntryV1 {
            repository_relative_filename: ".bbox/knowledge/knowledge-1.json".to_string(),
            encoded_bytes: KNOWLEDGE_BYTES.len() as u64,
            content_sha256: source_file_blob_sha256(KNOWLEDGE_BYTES),
        }
    }

    fn manifest(
        lane: SourceLaneV1,
        entries: &[SourceFileManifestEntryV1],
    ) -> SourceManifestDescriptorV1 {
        SourceManifestDescriptorV1 {
            manifest_sha256: source_manifest_sha256(lane, entries),
            file_count: entries.len() as u64,
            logical_bytes: entries.iter().map(|entry| entry.encoded_bytes).sum(),
            page_count: (!entries.is_empty()) as u64,
        }
    }

    fn publication_descriptor(
        scope: PublishedScope,
    ) -> bbox_knowledge_source::PublicationCandidateDescriptorV1 {
        let entries = vec![entry()];
        bbox_knowledge_source::PublicationCandidateDescriptorV1 {
            schema_version: SCHEMA_VERSION,
            scope,
            full_ref: "refs/heads/main".to_string(),
            publisher_commit: "1".repeat(40),
            object_format: GitObjectFormatV1::Sha1,
            knowledge: manifest(SourceLaneV1::Knowledge, &entries),
            gaps: manifest(SourceLaneV1::Gaps, &[]),
        }
    }

    fn provisional_descriptor(
        scope: PublishedScope,
        workspace_id: WorkspaceId,
    ) -> (
        bbox_knowledge_source::ProvisionalWorkspaceDescriptorV1,
        Vec<AncestryCommitV1>,
    ) {
        let root = "1".repeat(40);
        let nodes = vec![
            AncestryCommitV1 {
                commit_oid: root.clone(),
                parent_oids: Vec::new(),
            },
            AncestryCommitV1 {
                commit_oid: "2".repeat(40),
                parent_oids: vec![root.clone()],
            },
            AncestryCommitV1 {
                commit_oid: "3".repeat(40),
                parent_oids: vec![root],
            },
        ];
        let knowledge = vec![entry()];
        let knowledge_manifest = manifest(SourceLaneV1::Knowledge, &knowledge);
        let gaps_manifest = manifest(SourceLaneV1::Gaps, &[]);
        let working_pair = working_pair_sha256(&knowledge_manifest, &gaps_manifest);
        (
            bbox_knowledge_source::ProvisionalWorkspaceDescriptorV1 {
                schema_version: SCHEMA_VERSION,
                scope,
                workspace_id,
                sequence: 1,
                accepted_generation: "a".repeat(64),
                accepted_commit: "2".repeat(40),
                checkout_head: "3".repeat(40),
                merge_base: "1".repeat(40),
                object_format: GitObjectFormatV1::Sha1,
                ancestry: AncestryDescriptorV1 {
                    ancestry_sha256: ancestry_sha256(GitObjectFormatV1::Sha1, &nodes),
                    node_count: nodes.len() as u64,
                    edge_count: 2,
                    page_count: 1,
                },
                capture: StableCaptureV1 {
                    transaction_pending_before: false,
                    transaction_pending_after: false,
                    first_working_pair_sha256: working_pair.clone(),
                    second_working_pair_sha256: working_pair,
                },
                baseline_knowledge: knowledge_manifest.clone(),
                baseline_gaps: gaps_manifest.clone(),
                working_knowledge: knowledge_manifest,
                working_gaps: gaps_manifest,
            },
            nodes,
        )
    }

    #[tokio::test]
    async fn publication_routes_authenticate_before_parse_and_bind_project_and_producer() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let fixture = enabled_state(&root);
        let app = router(fixture.state.clone()).with_state(fixture.state);

        for token in [None, Some(fixture.other_producer_token.as_str())] {
            let response = app
                .clone()
                .oneshot(request(
                    "POST",
                    "/internal/knowledge-source/v1/publication/uploads",
                    token,
                    Body::from("{"),
                ))
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                if token.is_none() {
                    StatusCode::UNAUTHORIZED
                } else {
                    StatusCode::BAD_REQUEST
                }
            );
        }

        let forbidden = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/knowledge-source/v1/publication/uploads",
                Some(&fixture.other_producer_token),
                Body::from(
                    serde_json::to_vec(&BeginPublicationUploadRequestV1 {
                        descriptor: publication_descriptor(fixture.scope.clone()),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);

        let begun = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/knowledge-source/v1/publication/uploads",
                Some(&fixture.producer_token),
                Body::from(
                    serde_json::to_vec(&BeginPublicationUploadRequestV1 {
                        descriptor: publication_descriptor(fixture.scope.clone()),
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(begun.status(), StatusCode::CREATED);
        let begun: BeginSourceUploadResponseV1 =
            serde_json::from_slice(&to_bytes(begun.into_body(), 64 * 1024).await.unwrap()).unwrap();

        let hidden = app
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/internal/knowledge-source/v1/publication/uploads/{}/missing",
                    begun.upload_id
                ),
                Some(&fixture.other_producer_token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        let page = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/knowledge-source/v1/publication/uploads/{}/manifest/knowledge/0",
                    begun.upload_id
                ),
                Some(&fixture.producer_token),
                Body::from(
                    serde_json::to_vec(&SourceManifestPageV1 {
                        page_index: 0,
                        entries: vec![entry()],
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(page.status(), StatusCode::NO_CONTENT);
        let missing = app
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/internal/knowledge-source/v1/publication/uploads/{}/missing",
                    begun.upload_id
                ),
                Some(&fixture.producer_token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::OK);

        let hash = source_file_blob_sha256(KNOWLEDGE_BYTES);
        let blob = Request::builder()
            .method("PUT")
            .uri(format!(
                "/internal/knowledge-source/v1/publication/uploads/{}/blobs/{hash}",
                begun.upload_id
            ))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", fixture.producer_token),
            )
            .header(header::CONTENT_LENGTH, KNOWLEDGE_BYTES.len())
            .body(Body::from(KNOWLEDGE_BYTES))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(blob).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let finalized = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/knowledge-source/v1/publication/uploads/{}/finalize",
                    begun.upload_id
                ),
                Some(&fixture.producer_token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(finalized.status(), StatusCode::ACCEPTED);
    }

    #[tokio::test]
    async fn provisional_routes_require_live_exact_workspace_binding() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let fixture = enabled_state(&root);
        let app = router(fixture.state.clone()).with_state(fixture.state);
        for token in [None, Some(fixture.expired_workspace_token.as_str())] {
            let denied = app
                .clone()
                .oneshot(request(
                    "POST",
                    "/internal/knowledge-source/v1/provisional/uploads",
                    token,
                    Body::from("{"),
                ))
                .await
                .unwrap();
            assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
        }

        let (descriptor, nodes) =
            provisional_descriptor(fixture.scope.clone(), fixture.workspace_id.clone());
        let begun = app
            .clone()
            .oneshot(request(
                "POST",
                "/internal/knowledge-source/v1/provisional/uploads",
                Some(&fixture.workspace_token),
                Body::from(
                    serde_json::to_vec(&BeginProvisionalUploadRequestV1 { descriptor }).unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(begun.status(), StatusCode::CREATED);
        let begun: BeginSourceUploadResponseV1 =
            serde_json::from_slice(&to_bytes(begun.into_body(), 64 * 1024).await.unwrap()).unwrap();

        let hidden = app
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/internal/knowledge-source/v1/provisional/uploads/{}/missing",
                    begun.upload_id
                ),
                Some(&fixture.other_workspace_token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(hidden.status(), StatusCode::NOT_FOUND);

        let ancestry = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/knowledge-source/v1/provisional/uploads/{}/ancestry/0",
                    begun.upload_id
                ),
                Some(&fixture.workspace_token),
                Body::from(
                    serde_json::to_vec(&AncestryPageV1 {
                        page_index: 0,
                        nodes,
                    })
                    .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(ancestry.status(), StatusCode::NO_CONTENT);
        for class in ["baseline", "working"] {
            let page = app
                .clone()
                .oneshot(request(
                    "POST",
                    &format!(
                        "/internal/knowledge-source/v1/provisional/uploads/{}/manifest/{class}/knowledge/0",
                        begun.upload_id
                    ),
                    Some(&fixture.workspace_token),
                    Body::from(
                        serde_json::to_vec(&SourceManifestPageV1 {
                            page_index: 0,
                            entries: vec![entry()],
                        })
                        .unwrap(),
                    ),
                ))
                .await
                .unwrap();
            assert_eq!(page.status(), StatusCode::NO_CONTENT);
        }
        let missing = app
            .clone()
            .oneshot(request(
                "GET",
                &format!(
                    "/internal/knowledge-source/v1/provisional/uploads/{}/missing",
                    begun.upload_id
                ),
                Some(&fixture.workspace_token),
                Body::empty(),
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::OK);

        let hash = source_file_blob_sha256(KNOWLEDGE_BYTES);
        let blob = Request::builder()
            .method("PUT")
            .uri(format!(
                "/internal/knowledge-source/v1/provisional/uploads/{}/blobs/{hash}",
                begun.upload_id
            ))
            .header(
                header::AUTHORIZATION,
                format!("Bearer {}", fixture.workspace_token),
            )
            .header(header::CONTENT_LENGTH, KNOWLEDGE_BYTES.len())
            .body(Body::from(KNOWLEDGE_BYTES))
            .unwrap();
        assert_eq!(
            app.clone().oneshot(blob).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        let finalized = app
            .clone()
            .oneshot(request(
                "POST",
                &format!(
                    "/internal/knowledge-source/v1/provisional/uploads/{}/finalize",
                    begun.upload_id
                ),
                Some(&fixture.workspace_token),
                Body::from(
                    serde_json::to_vec(&FinalizeProvisionalUploadRequestV1 { lease_ttl_secs: 60 })
                        .unwrap(),
                ),
            ))
            .await
            .unwrap();
        assert_eq!(finalized.status(), StatusCode::ACCEPTED);
    }
}

//! Authenticated typed Git-history producer intake.
//!
//! The HTTP lane owns bounded parsing and streaming. Catalog authority stays
//! in `producer_auth`; durable resumability and graph verification stay in
//! `bbox-git-source-store`.

use std::io::SeekFrom;
use std::sync::Arc;

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

pub(crate) struct GitSourceRuntime {
    store: Arc<GitSourceStore>,
    activation_tx: std::sync::mpsc::SyncSender<String>,
    activation_rx: std::sync::Mutex<Option<std::sync::mpsc::Receiver<String>>>,
    /// Repo id -> `(activation-journal checksum, Tantivy searcher generation)`
    /// proven against the exact commit lane and durable snapshot receipts.
    /// Redrive reuses the proof only while both authorities are unchanged;
    /// any index commit forces a fresh exact-view probe.
    validated_activations: parking_lot::Mutex<std::collections::BTreeMap<String, (String, u64)>>,
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
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_git_source_request,
        ))
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
            ContractError::HistoryLimitExceeded | ContractError::HistoryRecordTooLarge => {
                Self::too_large(
                    "limit_exceeded",
                    "Git-source input exceeds an enforced limit",
                )
            }
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

    use axum::body::to_bytes;
    use axum::http::Request;
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
}

//! Authenticated knowledge and gap source intake, plus the workspace binding
//! registry.
//!
//! Publication candidates use the existing project-scoped producer grant; the
//! middleware boundary runs before Axum parses a request body.
//!
//! A workspace binding is the path-free capability a managed harness worker
//! presents on its daemon MCP session. It authorizes exactly two things for
//! that one checkout: the render locality exchange (`bbox_render` with the
//! bound-workspace selector) and the write-routing refusal that sends project
//! knowledge and gap writes to the harness's own checkout. It never selects
//! what a read returns: every read is published-only.

use std::collections::BTreeMap;
use std::io::SeekFrom;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
    BeginPublicationUploadRequestV1, BeginSourceUploadResponseV1, ContractError,
    FinalizeSourceUploadResponseV1, KnowledgeSourceLimits, MAX_MANIFEST_PAGE_BYTES,
    MAX_SOURCE_FILE_BYTES, MissingSourceBlobsPageV1, PublicationCandidateStatusV1,
    PublicationProbeRequestV1, PublicationProbeResponseV1, SourceLaneV1, SourceManifestPageV1,
};
use bbox_knowledge_source_store::{
    KnowledgeSourceStore, PublicationAuthorityV1, StoreLimits, StoreRequestError,
};
use bro_core::WorkspaceId;
use bro_rpc::ServiceToken;
use futures::StreamExt;
use serde::Deserialize;
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use super::SharedState;
use super::producer_auth::{ProducerGrant, RepoTransportGrantError};

const UPLOAD_BODY_TEMP_PREFIX: &str = ".knowledge-source-upload-body-";
const UPLOAD_BODY_TEMP_SUFFIX: &str = ".tmp";
const WORKSPACE_BINDING_TTL_SECS: u64 = 24 * 60 * 60;
/// How often a live task's binding has its expiry pushed out. Well inside
/// the TTL so one missed tick never expires a working session.
const WORKSPACE_BINDING_RENEW_INTERVAL_SECS: u64 = 60 * 60;
/// Store-root file of the retired operator-minted bindings. Removed once at
/// startup; nothing reads it.
const RETIRED_OPERATOR_BINDINGS_FILENAME: &str = "operator-workspace-bindings.json";

fn workspace_binding_token_sha256(secret: &str) -> [u8; 32] {
    use sha2::Digest as _;
    sha2::Sha256::digest(secret.as_bytes()).into()
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (l, r)| acc | (l ^ r))
        == 0
}

#[derive(Clone)]
pub(crate) struct WorkspaceBindingGrant {
    pub(crate) task_id: String,
    pub(crate) session_id: String,
    pub(crate) project_id: String,
    pub(crate) scope: bbox_corpus_core::identity::PublishedScope,
    pub(crate) workspace_id: WorkspaceId,
    pub(crate) expires_unix_secs: u64,
}

impl WorkspaceBindingGrant {
    pub(crate) fn is_live_now(&self) -> bool {
        self.expires_unix_secs > now_unix_secs()
    }
}

struct WorkspaceBindingEntry {
    /// SHA-256 of the 64-hex binding secret. Only the hash is held, in
    /// memory, so nothing daemon-side can reconstruct a token.
    token_sha256: [u8; 32],
    grant: WorkspaceBindingGrant,
}

impl WorkspaceBindingEntry {
    fn verify(&self, candidate: &str) -> bool {
        constant_time_eq(
            &self.token_sha256,
            &workspace_binding_token_sha256(candidate),
        )
    }
}

pub(crate) struct KnowledgeSourceRuntime {
    store: Arc<KnowledgeSourceStore>,
    workspace_bindings: parking_lot::RwLock<Vec<WorkspaceBindingEntry>>,
    /// Renewal cancellation handles keyed by the task id that owns the
    /// binding, beside the bindings themselves.
    workspace_binding_renewals: parking_lot::Mutex<BTreeMap<String, CancellationToken>>,
    /// Bounded record of what candidate acceptance last did per project.
    /// It lives beside the publication candidates it reacts to, so no new
    /// field has to be threaded through `SharedState`.
    acceptance: Arc<super::candidate_acceptance::CandidateAcceptanceLedger>,
}

pub(crate) struct KnowledgeTransportCheckoutPolicy {
    cutover: Arc<bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1>,
}

impl KnowledgeTransportCheckoutPolicy {
    pub(crate) fn new(
        cutover: Arc<
            bbox_indexing::knowledge_transport_cutover::KnowledgeTransportCutoverRuntimeV1,
        >,
    ) -> Self {
        Self { cutover }
    }
}

impl bbox_indexing::checkout_access::CheckoutAccessPolicy for KnowledgeTransportCheckoutPolicy {
    fn authorize(
        &self,
        request: &bbox_indexing::checkout_access::CheckoutAccessRequest,
    ) -> std::result::Result<(), bbox_indexing::checkout_access::CheckoutAccessError> {
        use bbox_indexing::checkout_access::{CheckoutAccessError, CheckoutAccessErrorCode};

        let governed_capability = matches!(
            request.kind,
            bbox_indexing::checkout_access::CheckoutAccessKind::PublisherConfigTreeRead
                | bbox_indexing::checkout_access::CheckoutAccessKind::KnowledgeGapOverlayRead
                | bbox_indexing::checkout_access::CheckoutAccessKind::ArtifactWatchDiscovery
                | bbox_indexing::checkout_access::CheckoutAccessKind::RepositoryMutation
        );
        if governed_capability && self.cutover.covers_project_str(&request.project_id) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::KnowledgeTransportAuthoritative,
                "error.knowledge_transport_authoritative: project checkout authority is closed by the knowledge transport cutover",
            ));
        }
        Ok(())
    }
}

impl KnowledgeSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config) -> Result<Self> {
        let root = config.paths.state_dir.join("knowledge-sources");
        retire_provisional_state(&root);
        let store = Arc::new(KnowledgeSourceStore::open(
            root,
            checked_store_limits(config)?,
        )?);
        reap_upload_body_tempfiles(store.root())?;
        Ok(Self {
            store,
            workspace_bindings: parking_lot::RwLock::new(Vec::new()),
            workspace_binding_renewals: parking_lot::Mutex::new(BTreeMap::new()),
            acceptance: Arc::new(super::candidate_acceptance::CandidateAcceptanceLedger::default()),
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
            workspace_binding_renewals: parking_lot::Mutex::new(BTreeMap::new()),
            acceptance: Arc::new(super::candidate_acceptance::CandidateAcceptanceLedger::default()),
        }
    }

    pub(crate) fn acceptance_ledger(
        &self,
    ) -> Arc<super::candidate_acceptance::CandidateAcceptanceLedger> {
        self.acceptance.clone()
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

    pub(crate) fn authenticate_workspace_binding(
        &self,
        candidate: &str,
        now: u64,
    ) -> Option<WorkspaceBindingGrant> {
        let mut matched = None;
        for entry in self.workspace_bindings.read().iter() {
            if entry.verify(candidate) && entry.grant.expires_unix_secs > now {
                matched = Some(entry.grant.clone());
            }
        }
        matched
    }

    pub(crate) fn authenticate_workspace_binding_now(
        &self,
        candidate: &str,
    ) -> Option<WorkspaceBindingGrant> {
        self.authenticate_workspace_binding(candidate, now_unix_secs())
    }

    fn install_workspace_binding_hashed(
        &self,
        token_sha256: [u8; 32],
        grant: WorkspaceBindingGrant,
        now: u64,
    ) {
        let mut bindings = self.workspace_bindings.write();
        bindings.retain(|entry| {
            entry.grant.expires_unix_secs > now
                && entry.grant.task_id != grant.task_id
                && entry.grant.session_id != grant.session_id
                && !constant_time_eq(&entry.token_sha256, &token_sha256)
        });
        bindings.push(WorkspaceBindingEntry {
            token_sha256,
            grant,
        });
    }

    #[cfg(test)]
    pub(crate) fn active_workspace_bindings(&self, now: u64) -> Vec<WorkspaceBindingGrant> {
        let mut bindings = self.workspace_bindings.write();
        bindings.retain(|entry| entry.grant.expires_unix_secs > now);
        bindings.iter().map(|entry| entry.grant.clone()).collect()
    }

    fn extend_workspace_binding(&self, task_id: &str, session_id: &str, now: u64) {
        for entry in self.workspace_bindings.write().iter_mut() {
            if entry.grant.task_id == task_id && entry.grant.session_id == session_id {
                entry.grant.expires_unix_secs = now.saturating_add(WORKSPACE_BINDING_TTL_SECS);
            }
        }
    }

    fn revoke_workspace_bindings(&self, task_id: &str) -> Vec<WorkspaceBindingGrant> {
        let mut revoked = Vec::new();
        self.workspace_bindings.write().retain(|entry| {
            if entry.grant.task_id == task_id {
                revoked.push(entry.grant.clone());
                false
            } else {
                true
            }
        });
        revoked
    }

    #[cfg(test)]
    fn install_workspace_binding(
        &self,
        token: ServiceToken,
        grant: WorkspaceBindingGrant,
        now: u64,
    ) {
        self.install_workspace_binding_hashed(
            workspace_binding_token_sha256(token.expose_secret()),
            grant,
            now,
        );
    }
}

/// Production adapter joining managed-checkout authority to the path-free
/// session capability retained by fleetd. It never persists or logs a token.
pub(crate) struct DaemonWorkspaceBindingAuthority {
    state: Arc<SharedState>,
}

impl DaemonWorkspaceBindingAuthority {
    pub(crate) fn new(state: Arc<SharedState>) -> Self {
        Self { state }
    }
}

/// Build the exact grant a managed worker spawn installs: catalog-resolved
/// project id, the worker-proved workspace identity, and the fixed binding TTL.
pub(crate) fn workspace_binding_grant(
    state: &SharedState,
    task_id: &str,
    session_id: &str,
    identity: &bro_protocol::WorkerWorkspaceIdentity,
) -> Result<WorkspaceBindingGrant> {
    let scope = bbox_corpus_core::identity::PublishedScope::try_new(
        identity.scope.repo_id().to_string(),
        identity.scope.bbox_root_relpath().to_string(),
    )?;
    let project_id = project_id_for_scope(state, &scope)?.ok_or_else(|| {
        anyhow::anyhow!("managed workspace scope is not present in current project authority")
    })?;
    Ok(WorkspaceBindingGrant {
        task_id: task_id.to_string(),
        session_id: session_id.to_string(),
        project_id,
        scope,
        workspace_id: identity.workspace_id.clone(),
        expires_unix_secs: now_unix_secs().saturating_add(WORKSPACE_BINDING_TTL_SECS),
    })
}

fn published_scopes(
    state: &SharedState,
) -> Result<Vec<bbox_corpus_core::identity::PublishedScope>> {
    let mut scopes = BTreeMap::<bbox_corpus_core::identity::PublishedScope, ()>::new();
    if let Some(store) = state.project_authority.catalog_store() {
        let snapshot = store.snapshot()?;
        for project in snapshot.catalog().projects.values() {
            if let bbox_corpus_core::project_catalog::ProjectScope::Published(scope) =
                &project.scope
            {
                scopes.insert(scope.clone(), ());
            }
        }
    } else {
        let records = state.records_provider.records_snapshot();
        for record in records.records.iter() {
            if let Some(scope) = super::checkout_access::published_scope_for_project(
                &state.checkout_access,
                &record.project_id,
            )? {
                scopes.insert(scope, ());
            }
        }
    }
    Ok(scopes.into_keys().collect())
}

/// Resolve one published scope to the single catalog project that owns it.
/// `Ok(None)` is an unregistered scope; an error is an ambiguous one.
pub(crate) fn project_id_for_scope(
    state: &SharedState,
    expected: &bbox_corpus_core::identity::PublishedScope,
) -> Result<Option<String>> {
    if let Some(store) = state.project_authority.catalog_store() {
        let snapshot = store.snapshot()?;
        let mut matched = None;
        for (project_id, project) in &snapshot.catalog().projects {
            if matches!(
                &project.scope,
                bbox_corpus_core::project_catalog::ProjectScope::Published(scope)
                    if scope == expected
            ) {
                if matched.replace(project_id.to_string()).is_some() {
                    bail!("managed workspace scope resolves to more than one catalog project");
                }
            }
        }
        return Ok(matched);
    }
    let records = state.records_provider.records_snapshot();
    super::checkout_access::project_id_for_published_scope(
        &state.checkout_access,
        records
            .records
            .iter()
            .map(|record| record.project_id.clone()),
        expected,
    )
}

/// Mint one fresh binding secret for an already-validated grant and install it.
/// This is the only place a workspace binding secret is generated.
pub(crate) fn mint_workspace_binding(
    state: &Arc<SharedState>,
    task_id: &str,
    session_id: &str,
    identity: &bro_protocol::WorkerWorkspaceIdentity,
) -> Result<crate::orchestration::MintedWorkspaceBinding> {
    let grant = workspace_binding_grant(state, task_id, session_id, identity)?;
    let secret = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let token = bro_protocol::WorkspaceBindingToken::parse(secret)?;
    let scope = grant.scope.clone();
    install_workspace_binding(state, grant, &token)?;
    Ok(crate::orchestration::MintedWorkspaceBinding { token, scope })
}

pub(crate) fn install_workspace_binding(
    state: &Arc<SharedState>,
    grant: WorkspaceBindingGrant,
    token: &bro_protocol::WorkspaceBindingToken,
) -> Result<()> {
    let token = ServiceToken::parse(token.expose_secret().to_string())?;
    install_workspace_binding_hashed(
        state,
        grant,
        workspace_binding_token_sha256(token.expose_secret()),
    )
}

fn install_workspace_binding_hashed(
    state: &Arc<SharedState>,
    grant: WorkspaceBindingGrant,
    token_sha256: [u8; 32],
) -> Result<()> {
    let runtime = tokio::runtime::Handle::try_current()
        .map_err(|_| anyhow::anyhow!("workspace binding requires an async runtime"))?;
    state.knowledge_sources.install_workspace_binding_hashed(
        token_sha256,
        grant.clone(),
        now_unix_secs(),
    );
    let cancellation = CancellationToken::new();
    if let Some(prior) = state
        .knowledge_sources
        .workspace_binding_renewals
        .lock()
        .insert(grant.task_id.clone(), cancellation.clone())
    {
        prior.cancel();
    }
    let state = state.clone();
    runtime.spawn(async move {
        loop {
            tokio::select! {
                _ = cancellation.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_secs(WORKSPACE_BINDING_RENEW_INTERVAL_SECS)) => {
                    state.knowledge_sources.extend_workspace_binding(
                        &grant.task_id,
                        &grant.session_id,
                        now_unix_secs(),
                    );
                }
            }
        }
    });
    Ok(())
}

/// Remove the state of the retired provisional snapshot transport from one
/// knowledge-source store root before the store opens: the provisional
/// generation tree, its finalize journals, and the operator binding records.
/// Idempotent and bounded to those fixed members. A failure is logged and
/// never stops the daemon; the store never reads the leftover state, and the
/// next start retries. Blobs only provisional manifests referenced are
/// unreferenced from here on, and store maintenance reclaims them after the
/// blob grace period.
fn retire_provisional_state(root: &std::path::Path) {
    match bbox_knowledge_source_store::retire_provisional_state(root) {
        Ok(report) if report.removed_directory || report.removed_journals > 0 => {
            tracing::info!(
                removed_directory = report.removed_directory,
                removed_journals = report.removed_journals,
                "removed retired provisional knowledge-source state"
            );
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(
            error = %format!("{error:#}"),
            "retired provisional knowledge-source state was not removed; it is never read"
        ),
    }
    let bindings = root.join(RETIRED_OPERATOR_BINDINGS_FILENAME);
    match std::fs::symlink_metadata(&bindings) {
        Ok(metadata) if !metadata.is_dir() => match std::fs::remove_file(&bindings) {
            Ok(()) => tracing::info!("removed retired operator workspace binding records"),
            Err(error) => tracing::warn!(
                error = %error,
                "retired operator workspace binding records were not removed; they are never read"
            ),
        },
        Ok(_) => tracing::warn!(
            "retired operator workspace binding path is a directory; left in place, never read"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            error = %error,
            "retired operator workspace binding records could not be inspected"
        ),
    }
}

impl crate::orchestration::WorkspaceBindingAuthority for DaemonWorkspaceBindingAuthority {
    fn candidate_scopes(&self) -> Result<Vec<bro_protocol::WorkerWorkspaceScope>> {
        published_scopes(&self.state)?
            .into_iter()
            .map(|scope| {
                bro_protocol::WorkerWorkspaceScope::try_new(
                    scope.repo_id().to_string(),
                    scope.bbox_root_relpath().to_string(),
                )
                .map_err(anyhow::Error::from)
            })
            .collect()
    }

    fn mint(
        &self,
        task_id: &str,
        session_id: &str,
        identity: &bro_protocol::WorkerWorkspaceIdentity,
    ) -> Result<crate::orchestration::MintedWorkspaceBinding> {
        mint_workspace_binding(&self.state, task_id, session_id, identity)
    }

    fn restore(
        &self,
        task_id: &str,
        session_id: &str,
        identity: &bro_protocol::WorkerWorkspaceIdentity,
        token: &bro_protocol::WorkspaceBindingToken,
    ) -> Result<()> {
        let grant = workspace_binding_grant(&self.state, task_id, session_id, identity)?;
        install_workspace_binding(&self.state, grant, token)
    }

    fn revoke_task(&self, task_id: &str) {
        if let Some(cancellation) = self
            .state
            .knowledge_sources
            .workspace_binding_renewals
            .lock()
            .remove(task_id)
        {
            cancellation.cancel();
        }
        self.state
            .knowledge_sources
            .revoke_workspace_bindings(task_id);
    }
}

impl super::BlackboxServer {
    pub(crate) fn observe_knowledge_transport_operation(
        &self,
        project_id: &str,
        operation: bbox_indexing::knowledge_transport_observations::KnowledgeTransportOperationV1,
        outcome: bbox_indexing::knowledge_transport_observations::KnowledgeTransportOutcomeV1,
    ) {
        if let Err(error) = self
            .state
            .knowledge_transport_observations
            .record(project_id, operation, outcome)
        {
            tracing::warn!(
                project_id,
                error = %error,
                "knowledge transport operation observation could not be persisted"
            );
        }
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
    {
        bail!("knowledge-source limits must be nonzero");
    }
    Ok(limits)
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    publication_router(state).layer(axum::middleware::from_fn(stamp_daemon_build_id))
}

/// Stamp every knowledge-source response, success or refusal, with this
/// daemon's build identity. Clients (the collectors) compare it against their own build id so a decode failure or a stuck
/// capture can be named as build skew instead of guessed at. Same source as
/// the roster's `daemon_build_id`.
async fn stamp_daemon_build_id(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    if let Ok(value) = header::HeaderValue::from_str(env!("BLACKBOX_BUILD_ID")) {
        response.headers_mut().insert(
            header::HeaderName::from_static(bbox_knowledge_source::DAEMON_BUILD_ID_HEADER),
            value,
        );
    }
    response
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
    Ok(Json(PublicationProbeResponseV1 {
        current,
        config_lane_supported: true,
    }))
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
    let lane = parse_publication_lane(&lane)?;
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
    let project_id = authority.project_id.clone();
    let response = {
        let authority = authority.clone();
        blocking(move || store.finalize_publication_upload(&authority, &upload_id)).await?
    };
    // The candidate is durable and Ready by the time finalize returns, so
    // this is where it gets its one acceptance attempt. With no pointer,
    // the first valid candidate from the owning producer establishes one.
    // With a pointer, a candidate on the bound producer, scope, and
    // configured ref advances it.
    //
    // The attempt runs before the response so a producer that polls status
    // immediately cannot observe an unserved Ready candidate that the
    // daemon was already about to accept. A refusal never fails the
    // finalize: the upload succeeded either way, and the prior accepted
    // generation keeps serving.
    {
        let state = state.clone();
        let source_generation_id = response.source_generation_id.clone();
        blocking(move || {
            let server = super::BlackboxServer::new(state);
            server.accept_ready_candidate(&project_id, &source_generation_id);
            Ok::<_, anyhow::Error>(())
        })
        .await?;
    }
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

/// Publication manifest lanes.
fn parse_publication_lane(value: &str) -> Result<SourceLaneV1, HttpError> {
    match value {
        "knowledge" => Ok(SourceLaneV1::Knowledge),
        "gaps" => Ok(SourceLaneV1::Gaps),
        "graphs" => Ok(SourceLaneV1::Graphs),
        "evidence" => Ok(SourceLaneV1::Evidence),
        "config" => Ok(SourceLaneV1::Config),
        _ => Err(HttpError::unprocessable(
            "knowledge_source_manifest_invalid",
            "knowledge-source lane is invalid",
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

pub(crate) fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
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
            | ContractError::GenerationLimitExceeded
            | ContractError::InvalidLimit => Self::too_large(
                "knowledge_source_limit_exceeded",
                "knowledge-source input exceeds an enforced limit",
            ),
            ContractError::UnsupportedSchema(_) => Self::unprocessable(
                "knowledge_source_contract_unsupported",
                "knowledge-source contract version is unsupported",
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
        GitObjectFormatV1, SCHEMA_VERSION, SourceFileManifestEntryV1, SourceManifestDescriptorV1,
        source_file_blob_sha256, source_manifest_sha256,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::server::producer_auth::ProducerAuthRuntime;
    use crate::server::state::catalog_fixture::CatalogFixture;

    /// First start on a deployed daemon: the retired provisional store, its
    /// journals, and the operator binding records are removed before the
    /// store opens; a second start finds nothing and changes nothing; the
    /// store then opens and maintains without reading any of it.
    #[test]
    fn startup_removes_retired_provisional_state_once() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("knowledge-sources");
        // Never provisioned: an absent root is not an error.
        retire_provisional_state(&root);
        assert!(!root.exists());

        let legacy = root.join("provisional/generations/p_one/0123456789abcdef0123456789abcdef");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::write(legacy.join("current.json"), b"{\"legacy\":true}").unwrap();
        std::fs::create_dir_all(root.join("provisional/uploads")).unwrap();
        std::fs::create_dir_all(root.join("journals")).unwrap();
        std::fs::write(
            root.join("journals/provisional-kws_legacy.json"),
            b"{\"kind\":\"provisional\"}",
        )
        .unwrap();
        std::fs::write(
            root.join(RETIRED_OPERATOR_BINDINGS_FILENAME),
            b"{\"version\":1,\"bindings\":[]}",
        )
        .unwrap();

        retire_provisional_state(&root);
        assert!(!root.join("provisional").exists());
        assert!(!root.join("journals/provisional-kws_legacy.json").exists());
        assert!(!root.join(RETIRED_OPERATOR_BINDINGS_FILENAME).exists());

        // Idempotent: the second start is a no-op.
        retire_provisional_state(&root);
        let store = KnowledgeSourceStore::open(&root, StoreLimits::default()).unwrap();
        store.maintain(&std::collections::BTreeSet::new()).unwrap();
        assert!(!root.join("provisional").exists());
    }

    /// The publication manifest route admits every lane, including the
    /// configuration lane, and refuses an unknown segment.
    #[test]
    fn publication_lane_segments_are_exact() {
        for (segment, lane) in [
            ("knowledge", SourceLaneV1::Knowledge),
            ("gaps", SourceLaneV1::Gaps),
            ("graphs", SourceLaneV1::Graphs),
            ("evidence", SourceLaneV1::Evidence),
            ("config", SourceLaneV1::Config),
        ] {
            assert_eq!(parse_publication_lane(segment).unwrap(), lane);
        }
        assert!(parse_publication_lane("configuration").is_err());
    }

    const KNOWLEDGE_BYTES: &[u8] = br#"{"id":"knowledge-1"}"#;

    struct TestAuthority {
        /// Owns the catalog tempdir for the fixture's lifetime.
        _catalog_fixture: crate::server::state::catalog_fixture::CatalogFixture,
        state: Arc<SharedState>,
        producer_token: String,
        other_producer_token: String,
        scope: PublishedScope,
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

    fn enabled_state(_root: &std::path::Path) -> TestAuthority {
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
        let catalog_fixture = crate::server::state::catalog_fixture::CatalogFixture::new();
        catalog_fixture.add_published_project(project_id.as_str(), &scope);
        catalog_fixture.add_published_project(other_project_id.as_str(), &other_scope);
        catalog_fixture.install_publication(
            project_id.as_str(),
            &scope,
            crate::server::state::catalog_fixture::COMMIT_TWO,
            &[],
            &[],
        );
        let state = catalog_fixture.server().state.clone();
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

        TestAuthority {
            _catalog_fixture: catalog_fixture,
            state,
            producer_token,
            other_producer_token,
            scope,
        }
    }

    #[test]
    fn workspace_binding_replacement_is_exact_and_expiry_is_enforced() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = KnowledgeSourceRuntime::for_test(directory.path());
        let workspace_id = WorkspaceId::parse("0123456789abcdef0123456789abcdef").unwrap();
        let grant = |session_id: &str, expires_unix_secs| WorkspaceBindingGrant {
            task_id: "task-one".to_string(),
            session_id: session_id.to_string(),
            project_id: "p_00000000000000000000000000000001".to_string(),
            scope: PublishedScope::try_new("repo-one", ".").unwrap(),
            workspace_id: workspace_id.clone(),
            expires_unix_secs,
        };
        let first = "6".repeat(64);
        runtime.install_workspace_binding(
            ServiceToken::parse(first.clone()).unwrap(),
            grant("session-one", 200),
            100,
        );
        assert!(
            runtime
                .authenticate_workspace_binding(&first, 100)
                .is_some()
        );

        let replacement = "7".repeat(64);
        runtime.install_workspace_binding(
            ServiceToken::parse(replacement.clone()).unwrap(),
            grant("session-two", 200),
            100,
        );
        assert!(
            runtime
                .authenticate_workspace_binding(&first, 100)
                .is_none()
        );
        let restored = runtime
            .authenticate_workspace_binding(&replacement, 100)
            .unwrap();
        assert_eq!(restored.task_id, "task-one");
        assert_eq!(restored.session_id, "session-two");
        assert_eq!(runtime.active_workspace_bindings(100).len(), 1);
        assert_eq!(runtime.revoke_workspace_bindings("task-one").len(), 1);
        assert!(
            runtime
                .authenticate_workspace_binding(&replacement, 100)
                .is_none()
        );
        runtime.install_workspace_binding(
            ServiceToken::parse(replacement.clone()).unwrap(),
            grant("session-two", 200),
            100,
        );
        assert!(
            runtime
                .authenticate_workspace_binding(&replacement, 200)
                .is_none()
        );
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
            graphs: SourceManifestDescriptorV1::default(),
            evidence: SourceManifestDescriptorV1::default(),
            config: None,
        }
    }

    #[tokio::test]
    async fn every_knowledge_source_response_carries_the_daemon_build_id() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let fixture = enabled_state(&root);
        let app = router(fixture.state.clone()).with_state(fixture.state.clone());

        // A refusal (unauthenticated) and a parse failure both carry it: the
        // stamp sits outside the auth layer so skew is nameable even when the
        // request never reaches a handler.
        for (path, token) in [
            ("/internal/knowledge-source/v1/publication/uploads", None),
            (
                "/internal/knowledge-source/v1/publication/probe",
                Some(fixture.other_producer_token.as_str()),
            ),
        ] {
            let response = app
                .clone()
                .oneshot(request("POST", path, token, Body::from("{")))
                .await
                .unwrap();
            assert_eq!(
                response
                    .headers()
                    .get(bbox_knowledge_source::DAEMON_BUILD_ID_HEADER)
                    .and_then(|value| value.to_str().ok()),
                Some(env!("BLACKBOX_BUILD_ID")),
                "{path} response must carry the daemon build id"
            );
        }
    }

    #[tokio::test]
    async fn publication_routes_authenticate_before_parse_and_bind_project_and_producer() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let fixture = enabled_state(&root);
        let app = router(fixture.state.clone()).with_state(fixture.state.clone());

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

    /// Finalize is where a Ready candidate is accepted: an admitted project
    /// with no pointer is established by its first valid candidate, with no
    /// grant anywhere.
    #[tokio::test]
    async fn finalize_establishes_the_first_valid_candidate() {
        let (_fixture, state, project_id) = finalize_candidate_for_acceptance(true).await;
        let attempt = state
            .knowledge_sources
            .acceptance_ledger()
            .last_attempt(project_id.as_str())
            .expect("the acceptance is recorded for publisher status");
        assert!(attempt.outcome.accepted(), "{attempt:?}");
        let installed = state
            .accepted_publications
            .as_ref()
            .unwrap()
            .installed_pointer(&project_id)
            .unwrap()
            .expect("the first valid candidate establishes the pointer");
        assert_eq!(installed.full_ref, "refs/heads/main");
        assert_eq!(installed.source.kind(), "producer");
    }

    /// A project that was never admitted (no repo-knowledge capable
    /// attachment) is not established, and the refusal is recorded without
    /// failing the upload.
    #[tokio::test]
    async fn acceptance_refusal_is_recorded_without_failing_finalize() {
        let (_fixture, state, project_id) = finalize_candidate_for_acceptance(false).await;
        let attempt = state
            .knowledge_sources
            .acceptance_ledger()
            .last_attempt(project_id.as_str())
            .expect("the refusal is recorded for publisher status");
        assert_eq!(
            attempt.outcome,
            crate::server::candidate_acceptance::AcceptanceOutcome::NoAttachedCheckout
        );
        assert_eq!(
            state
                .accepted_publications
                .as_ref()
                .unwrap()
                .installed_pointer(&project_id)
                .unwrap(),
            None
        );
    }

    async fn finalize_candidate_for_acceptance(
        repo_knowledge: bool,
    ) -> (CatalogFixture, Arc<SharedState>, ProjectId) {
        use std::io::Cursor;

        use bbox_corpus_core::project_catalog::{
            AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus,
            CheckoutAttachment,
        };

        let catalog_fixture = CatalogFixture::new();
        let scope = CatalogFixture::scope(".");
        let project_id = ProjectId::parse("p_acceptance_finalize").unwrap();
        catalog_fixture.add_published_project(project_id.as_str(), &scope);
        let attachment_id = AttachmentId::parse("att_bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").unwrap();
        let checkout_dir = catalog_fixture
            .root()
            .join("remote-checkout")
            .to_string_lossy()
            .into_owned();
        let epoch = catalog_fixture.store().snapshot().unwrap().epoch();
        catalog_fixture
            .store()
            .transact(epoch, |_catalog, attachments| {
                attachments.attachments.insert(
                    attachment_id.clone(),
                    CheckoutAttachment {
                        attachment_id: attachment_id.clone(),
                        project_id: project_id.clone(),
                        checkout_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                        checkout_dir: checkout_dir.clone(),
                        checkout_project_dir: checkout_dir.clone(),
                        project_root_relpath: ".".into(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: Some("main".into()),
                        capabilities: AttachmentCapabilities {
                            repo_knowledge,
                            ..Default::default()
                        },
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-09-23T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        let state = catalog_fixture.server().state.clone();
        let producer_id = "acceptance-producer";
        let grant = ProducerGrant {
            producer_id: producer_id.into(),
            projects: BTreeMap::from([(scope.clone(), project_id.as_str().to_string())]),
        };
        let catalog = catalog_fixture
            .store()
            .snapshot()
            .unwrap()
            .catalog()
            .clone();
        state
            .code_sources
            .install_auth_for_test(Arc::new(ProducerAuthRuntime::for_test_catalog(
                vec![(ServiceToken::parse("6".repeat(64)).unwrap(), grant.clone())],
                catalog.as_ref(),
            )));

        let store = state.knowledge_sources.store();
        let authority = PublicationAuthorityV1 {
            producer_id: producer_id.into(),
            project_id: project_id.as_str().to_string(),
            scope: scope.clone(),
        };
        // A normalizable knowledge entry, so the only thing that can stop
        // acceptance is the admission check under test.
        let knowledge_bytes = serde_json::to_vec(
            &crate::server::state::catalog_fixture::knowledge_entry("knowledge-1", "accepted"),
        )
        .unwrap();
        let manifest_entry = SourceFileManifestEntryV1 {
            repository_relative_filename: ".bbox/knowledge/knowledge-1.json".to_string(),
            encoded_bytes: knowledge_bytes.len() as u64,
            content_sha256: source_file_blob_sha256(&knowledge_bytes),
        };
        let mut descriptor = publication_descriptor(scope);
        descriptor.knowledge = manifest(
            SourceLaneV1::Knowledge,
            std::slice::from_ref(&manifest_entry),
        );
        let upload = store
            .begin_publication_upload(&authority, descriptor)
            .unwrap();
        store
            .put_publication_manifest_page(
                &authority,
                &upload.upload_id,
                SourceLaneV1::Knowledge,
                0,
                &SourceManifestPageV1 {
                    page_index: 0,
                    entries: vec![manifest_entry.clone()],
                },
            )
            .unwrap();
        store
            .missing_publication_blobs(&authority, &upload.upload_id, None)
            .unwrap();
        store
            .install_publication_blob(
                &authority,
                &upload.upload_id,
                &manifest_entry.content_sha256,
                manifest_entry.encoded_bytes,
                Cursor::new(knowledge_bytes),
            )
            .unwrap();

        let (status, _) = finalize_publication_upload(
            State(state.clone()),
            Extension(grant),
            Path(upload.upload_id),
        )
        .await
        .expect("acceptance never fails finalize");
        assert_eq!(status, StatusCode::ACCEPTED);
        // The fixture owns the catalog tempdir, so the caller holds it.
        (catalog_fixture, state, project_id)
    }
}

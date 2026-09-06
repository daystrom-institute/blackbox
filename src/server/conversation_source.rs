//! The `/internal/conversation-source/v1/*` ingest route layer.
//!
//! Mounted beside [`super::file_source`]'s router and shaped the same way: one
//! authenticated middleware in front of the whole family, one handler per wire
//! verb, and a single error taxonomy rendering
//! [`bbox_conversation_source::ErrorResponse`]. Route separation exists so
//! producer grants, limits, and health can be reasoned about per lane.
//!
//! Five properties are load-bearing:
//!
//! 1. **Authorization is by connector scope AND by lane.** Every handler
//!    reaches a scope before it reaches the store, and the scope comes from the
//!    request body (the POST verbs) or the query string (the GET verbs). A
//!    grant carries which lane it opens, so a file-lane grant reaching these
//!    routes is refused with `scope_wrong_lane` even though it authenticates
//!    through the same producer table.
//!
//! 2. **The pending-onboarding admission is exclusive to the onboard route.**
//!    A configured scope with no catalog project is admitted there and refused
//!    everywhere else, which is the code-collection precedent unchanged.
//!
//! 3. **Nothing blocks on the request path.** [`ConversationSourceStore`] is
//!    synchronous by design (append, fsync, atomic rename, parent fsync), so
//!    every call into it crosses [`blocking`], which is `spawn_blocking`. The
//!    `clippy.toml` disallowed-methods gate and `scripts/lint-concurrency.sh`
//!    both police this.
//!
//! 4. **Bodies are bounded before they are parsed.** Each route carries its
//!    own `DefaultBodyLimit` from the leaf crate's caps, and the leaf
//!    contract's per-record and per-batch limits are re-checked after parsing,
//!    so an authenticated producer still does not get to decide how much work
//!    one request commits.
//!
//! 5. **There is no socket to any chat provider anywhere in this file, or
//!    anywhere else in the daemon.** This lane is pure acceptance. Observation
//!    happens producer-side, on the host that holds the credential; the corpus
//!    host is a receiver. That is the whole point of the collector template and
//!    design section 2 rejects the alternative by name.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::extract::{DefaultBodyLimit, Extension, Json, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bbox_conversation_source::{
    ChannelRosterReceiptV1, ChannelRosterRequestV1, ConversationBatchReceiptV1,
    ConversationBatchV1, ConversationCatalogOnboardRequestV1, ConversationCatalogOnboardResponseV1,
    ConversationCursorsResponseV1, ConversationRevisionsReceiptV1, ConversationRevisionsRequestV1,
    ConversationSourceError, ConversationStatusResponseV1, ErrorResponse,
};
use bbox_conversation_source_store::ConversationSourceStore;
use bbox_corpus_core::project_catalog::ConnectorScope;
use serde::Deserialize;

use super::SharedState;
use super::connector_grants::ConnectorGrantRuntime;
use super::producer_auth::ConnectorGrant;

/// The state-dir subdirectory this lane owns. Sibling of `file-sources` and
/// `code-sources`, separate because the durable shape, the key, and the
/// retention story are all this lane's own.
const CONVERSATION_SOURCE_DIR: &str = "conversation-sources";

/// One authenticated producer contact, held in memory only.
#[derive(Clone)]
pub(crate) struct ProducerContact {
    pub(crate) last_seen_epoch_secs: i64,
    pub(crate) user_agent: String,
}

/// Last authenticated contact per conversation scope.
///
/// Boot-scoped by design: the question this answers is "is the satellite
/// alive NOW", not "when did it ever run", and a durable answer would outlive
/// the daemon it describes. Recording happens only on the INGEST verbs
/// (/batches, /channels, /revisions, /cursors) and only after the grant check
/// passes, so the map never names a scope an unauthorized caller asked about
/// and no bearer-holding poller can refresh a stale satellite into looking
/// alive: the status read renders the map without touching it.
pub(crate) struct ProducerPresence {
    contacts: std::sync::Mutex<std::collections::BTreeMap<ConnectorScope, ProducerContact>>,
    /// When this daemon started, so "never seen" can wait out a boot grace
    /// before it is reported: a restart must not page one row per granted
    /// scope for scopes whose satellites simply have not polled yet.
    boot_epoch_secs: i64,
}

impl Default for ProducerPresence {
    fn default() -> Self {
        Self {
            contacts: std::sync::Mutex::new(std::collections::BTreeMap::new()),
            boot_epoch_secs: chrono::Utc::now().timestamp(),
        }
    }
}

impl ProducerPresence {
    /// Record a contact and hand back what was recorded, so the caller can
    /// log or assert it without taking the lock a second time.
    fn note(
        &self,
        scope: &ConnectorScope,
        user_agent: &str,
        now_epoch_secs: i64,
    ) -> ProducerContact {
        let contact = ProducerContact {
            last_seen_epoch_secs: now_epoch_secs,
            user_agent: user_agent.to_string(),
        };
        self.contacts
            .lock()
            .unwrap()
            .insert(scope.clone(), contact.clone());
        contact
    }

    fn contact(&self, scope: &ConnectorScope) -> Option<ProducerContact> {
        self.contacts.lock().unwrap().get(scope).cloned()
    }

    fn boot_epoch_secs(&self) -> i64 {
        self.boot_epoch_secs
    }
}

/// Render a contact stamp the way the wire and the operators read it: RFC3339,
/// whole seconds. Unrepresentable epochs (a far-future client clock, say)
/// render as the epoch, never as a panic.
pub(crate) fn contact_rfc3339(epoch_secs: i64) -> String {
    chrono::DateTime::from_timestamp(epoch_secs, 0)
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

/// Runtime holder for the conversation landing store.
///
/// One field, like [`super::file_source::FileSourceRuntime`], and for the same
/// reason: this lane's authorization lives in the connector grant table so
/// there is exactly one place grants are resolved, and there is no activation
/// or cutback machinery here at all.
pub(crate) struct ConversationSourceRuntime {
    store: Arc<ConversationSourceStore>,
    presence: ProducerPresence,
}

impl ConversationSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config) -> Result<Self> {
        let store =
            ConversationSourceStore::open(config.paths.state_dir.join(CONVERSATION_SOURCE_DIR))?;
        Ok(Self {
            store: Arc::new(store),
            presence: ProducerPresence::default(),
        })
    }

    pub(crate) fn store(&self) -> Arc<ConversationSourceStore> {
        self.store.clone()
    }

    /// Record an authenticated contact for the status surface and the
    /// silence signal. Handlers call this after the grant check on the ingest
    /// verbs only; the status route reads and never records, so a poller can
    /// never refresh staleness. The presence tests age contacts through
    /// `ProducerPresence::note`'s injected clock, not through this wrapper.
    pub(crate) fn note_producer_contact(&self, scope: &ConnectorScope, user_agent: &str) {
        self.presence
            .note(scope, user_agent, chrono::Utc::now().timestamp());
    }

    /// Last contact for one scope, as the silence signal needs it: raw epoch
    /// seconds, so the threshold comparison happens against one clock.
    pub(crate) fn producer_contact(&self, scope: &ConnectorScope) -> Option<ProducerContact> {
        self.presence.contact(scope)
    }

    /// When this daemon booted, so "never seen since boot" can wait out a
    /// grace period instead of firing on every restart.
    pub(crate) fn producer_boot_epoch_secs(&self) -> i64 {
        self.presence.boot_epoch_secs()
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        Self {
            store: Arc::new(
                ConversationSourceStore::open(root.join(CONVERSATION_SOURCE_DIR)).unwrap(),
            ),
            presence: ProducerPresence::default(),
        }
    }
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/conversation-source/v1/catalog/onboard",
            post(catalog_onboard).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/conversation-source/v1/channels",
            post(post_channels).layer(DefaultBodyLimit::max(
                bbox_conversation_source::MAX_ROSTER_BODY_BYTES,
            )),
        )
        .route("/internal/conversation-source/v1/cursors", get(get_cursors))
        .route(
            "/internal/conversation-source/v1/batches",
            post(post_batch).layer(DefaultBodyLimit::max(
                bbox_conversation_source::MAX_BATCH_BODY_BYTES,
            )),
        )
        .route(
            "/internal/conversation-source/v1/revisions",
            post(post_revisions).layer(DefaultBodyLimit::max(
                bbox_conversation_source::MAX_REVISIONS_BODY_BYTES,
            )),
        )
        .route("/internal/conversation-source/v1/status", get(get_status))
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_conversation_source_request,
        ))
}

// ---------------------------------------------------------------------------
// Authorization helpers
// ---------------------------------------------------------------------------

fn connectors(state: &SharedState) -> Arc<ConnectorGrantRuntime> {
    state.code_sources.producer_auth().connectors().clone()
}

/// Require that this producer holds a grant on this scope, on THIS lane.
///
/// The three refusals are deliberately distinct because they are three
/// different fixes: `scope_forbidden` means the operator never granted it,
/// `scope_wrong_lane` means the grant exists but names the other endpoint
/// family, and `scope_pending_onboarding` means it was granted and never
/// onboarded. Collapsing any two sends an operator to edit the wrong file.
fn require_ingest_scope(
    connectors: &ConnectorGrantRuntime,
    grant: &ConnectorGrant,
    scope: &ConnectorScope,
) -> Result<(), HttpError> {
    require_granted_lane(connectors, grant, scope)?;
    if connectors.is_pending_onboard(scope) {
        return Err(HttpError::new(
            StatusCode::CONFLICT,
            "scope_pending_onboarding",
            "connector scope has no catalog project yet; onboard it before landing records",
        ));
    }
    Ok(())
}

/// Grant membership plus the lane check, without the onboarding requirement.
///
/// Split out because the onboard route needs exactly this much and no more: it
/// is the one route a pending scope may reach, and it still must not be
/// reachable by a producer that was never granted the scope or that holds it on
/// the file lane.
fn require_granted_lane(
    connectors: &ConnectorGrantRuntime,
    grant: &ConnectorGrant,
    scope: &ConnectorScope,
) -> Result<(), HttpError> {
    let granted = connectors.grants().iter().any(|expectation| {
        expectation.producer_id == grant.producer_id && &expectation.scope == scope
    });
    if !granted {
        return Err(HttpError::new(
            StatusCode::FORBIDDEN,
            "scope_forbidden",
            "connector scope is not authorized for this producer",
        ));
    }
    if connectors.profile_for(scope.connector_source_id())
        != Some(crate::config::ConnectorProfile::Conversation)
    {
        return Err(HttpError::new(
            StatusCode::FORBIDDEN,
            "scope_wrong_lane",
            "connector scope is not granted on the conversation-source lane",
        ));
    }
    Ok(())
}

/// The scope selector the two GET verbs carry.
///
/// Both halves are required. A `connector_source_id` alone would let a producer
/// address a scope whose kind it does not know, and the grant comparison is on
/// the whole [`ConnectorScope`], so accepting a partial selector would mean
/// reconstructing the missing half from the grant table, which is the server
/// guessing what the producer meant.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScopeQuery {
    connector_source_id: String,
    connector_kind: String,
}

impl ScopeQuery {
    fn scope(&self) -> Result<ConnectorScope, HttpError> {
        ConnectorScope::try_new(
            self.connector_source_id.clone(),
            self.connector_kind.clone(),
        )
        .map_err(|error| {
            HttpError::unprocessable(
                "invalid_conversation_source_input",
                format!("invalid connector scope selector: {error}"),
            )
        })
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// The producer's own name for itself, verbatim up to a bound. Empty when
/// absent: presence is the fact being recorded, and a producer that sends no
/// User-Agent is still present. The bound keeps a misbehaving or hostile
/// header from turning an in-memory map entry (and every status response and
/// inbox row that renders it) into unbounded storage.
fn user_agent(headers: &axum::http::HeaderMap) -> String {
    headers
        .get(axum::http::header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .chars()
        .take(256)
        .collect()
}

async fn post_channels(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ChannelRosterRequestV1>,
) -> Result<impl IntoResponse, HttpError> {
    // Contract validation FIRST, before the grant check touches the scope: a
    // malformed scope is a contract error, not an authorization one, and
    // reporting it as 403 would send an operator hunting a grant that is fine.
    request.validate().map_err(HttpError::from_contract)?;
    require_ingest_scope(&connectors(&state), &grant, &request.scope)?;
    state
        .conversation_sources
        .note_producer_contact(&request.scope, &user_agent(&headers));

    let store = state.conversation_sources.store();
    let workspace_id = request.workspace_id.clone();
    // The clock is read HERE, mirroring `get_status`: the store holds no time
    // dependency, and this is the stamp a synthesized unenrollment
    // observation carries when `request.complete` asks for one.
    let swept_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
    let recorded = blocking(move || {
        store.record_roster(
            &request.scope,
            &request.workspace_id,
            &request.channels,
            request.complete,
            &swept_at,
        )
    })
    .await?;
    Ok((
        StatusCode::OK,
        Json(ChannelRosterReceiptV1 {
            workspace_id,
            channels_recorded: recorded,
        }),
    ))
}

async fn post_batch(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    headers: axum::http::HeaderMap,
    Json(batch): Json<ConversationBatchV1>,
) -> Result<Json<ConversationBatchReceiptV1>, HttpError> {
    batch.validate().map_err(HttpError::from_contract)?;
    require_ingest_scope(&connectors(&state), &grant, &batch.scope)?;
    state
        .conversation_sources
        .note_producer_contact(&batch.scope, &user_agent(&headers));

    let store = state.conversation_sources.store();
    // The store re-validates and, crucially, checks the batch's workspace
    // against the workspace THIS scope is bound to. That second check is the
    // one the route layer cannot make: the binding is durable state, not
    // config.
    let receipt = blocking(move || store.land_batch(&batch.scope, &batch)).await?;
    Ok(Json(receipt))
}

async fn post_revisions(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    headers: axum::http::HeaderMap,
    Json(request): Json<ConversationRevisionsRequestV1>,
) -> Result<Json<ConversationRevisionsReceiptV1>, HttpError> {
    request.validate().map_err(HttpError::from_contract)?;
    require_ingest_scope(&connectors(&state), &grant, &request.scope)?;
    state
        .conversation_sources
        .note_producer_contact(&request.scope, &user_agent(&headers));

    let store = state.conversation_sources.store();
    let receipt = blocking(move || store.land_revisions(&request.scope, &request)).await?;
    Ok(Json(receipt))
}

async fn get_cursors(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    headers: axum::http::HeaderMap,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<ConversationCursorsResponseV1>, HttpError> {
    let scope = query.scope()?;
    require_ingest_scope(&connectors(&state), &grant, &scope)?;
    state
        .conversation_sources
        .note_producer_contact(&scope, &user_agent(&headers));

    let store = state.conversation_sources.store();
    let owned = scope.clone();
    let (workspace_id, channels) = blocking(move || {
        let workspace_id = store
            .workspace_binding(&owned)?
            .map(|binding| binding.workspace_id);
        Ok((workspace_id, store.cursors(&owned)?))
    })
    .await?;
    Ok(Json(ConversationCursorsResponseV1 {
        schema_version: bbox_conversation_source::SCHEMA_VERSION,
        scope,
        workspace_id,
        channels,
    }))
}

async fn get_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Query(query): Query<ScopeQuery>,
) -> Result<Json<ConversationStatusResponseV1>, HttpError> {
    let scope = query.scope()?;
    require_ingest_scope(&connectors(&state), &grant, &scope)?;

    // Render WITHOUT recording: the status read is a read. If it refreshed
    // presence, last_seen_at would always be "now" and any bearer-holding
    // poller could suppress the inbox silence row for a satellite that is
    // actually down. Only the ingest verbs are contacts.
    let producer = state
        .conversation_sources
        .producer_contact(&scope)
        .map(|contact| bbox_conversation_source::ProducerContactV1 {
            last_seen_at: contact_rfc3339(contact.last_seen_epoch_secs),
            user_agent: contact.user_agent,
        });

    let store = state.conversation_sources.store();
    let owned = scope.clone();
    // The clock is read HERE and passed in, so the store holds no time
    // dependency and lag is a pure function of the journal plus one number.
    let now = chrono::Utc::now().timestamp();
    let (workspace_id, channels) = blocking(move || {
        let workspace_id = store
            .workspace_binding(&owned)?
            .map(|binding| binding.workspace_id);
        Ok((workspace_id, store.status(&owned, now)?))
    })
    .await?;
    Ok(Json(ConversationStatusResponseV1 {
        schema_version: bbox_conversation_source::SCHEMA_VERSION,
        scope,
        workspace_id,
        channels,
        producer,
    }))
}

async fn catalog_onboard(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<ConversationCatalogOnboardRequestV1>,
) -> Result<impl IntoResponse, HttpError> {
    request.validate().map_err(HttpError::from_contract)?;

    // The pending-onboarding admission, and the ONLY place it applies on this
    // lane. Grant membership and the lane are still required; only the catalog
    // project requirement is waived.
    let connectors = connectors(&state);
    require_granted_lane(&connectors, &grant, &request.scope)?;

    let store = state
        .project_authority
        .catalog_store()
        .cloned()
        .ok_or_else(|| {
            HttpError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "catalog_unavailable",
                "the project catalog is unavailable",
            )
        })?;
    let grants: Vec<_> = connectors.grants().to_vec();
    let producer_id = grant.producer_id.clone();
    let onboard_request = request.clone();
    let receipt = blocking(move || -> Result<ConversationCatalogOnboardResponseV1> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        // The SAME composite the file lane drives. A conversation source's
        // "remote root" is its workspace, which is exactly what the composite's
        // probed-root field means: the coordinate the producer actually
        // reached, checked against the operator's declared expectation.
        let onboard = bbox_indexing::project_catalog_admin::ConnectorOnboardRequest {
            producer_id,
            scope: onboard_request.scope.clone(),
            probed_connector_kind: onboard_request.probed_connector_kind.clone(),
            probed_remote_authority: onboard_request.probed_remote_authority.clone(),
            probed_remote_root_id: Some(onboard_request.probed_workspace_id.clone()),
            probed_remote_display_name: onboard_request.probed_workspace_display_name.clone(),
            display_name: onboard_request.display_name.clone(),
            observed_at: now,
        };
        let epoch = store
            .snapshot()
            .map_err(|error| anyhow!("{error}"))?
            .epoch();
        let receipt = bbox_indexing::project_catalog_admin::connector_onboard(
            &store, epoch, &grants, &onboard,
        )
        .map_err(|error| anyhow!("{error}"))?;
        Ok(ConversationCatalogOnboardResponseV1 {
            project_id: receipt.project_id.as_str().to_string(),
            created_project: receipt.created,
            epoch: receipt.catalog_epoch,
        })
    })
    .await?;

    // Bind the workspace AFTER the catalog project exists, so a durable
    // workspace binding never names a scope the catalog does not hold. The bind
    // is idempotent for the same workspace and refuses a second one, which is
    // the durable half of the tamper check every batch is measured against.
    let conversation_store = state.conversation_sources.store();
    let scope = request.scope.clone();
    let workspace_id = request.probed_workspace_id.clone();
    blocking(move || {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        conversation_store.bind_workspace(&scope, &workspace_id, &now)
    })
    .await?;

    // Promote the freshly cataloged scope out of `pending_onboard` so the
    // ingest routes stop refusing it.
    if receipt.created_project {
        let config = state.config.read().clone();
        let projects = state.records_provider.records_snapshot().records;
        if let Err(error) = state.code_sources.reload(&config, &projects) {
            tracing::warn!(
                error = %error,
                "post-onboard connector grant reload failed; the scope is admitted on the \
                 next reload"
            );
        }
    }
    state.nudge_edge_index_rebuild();
    Ok((StatusCode::CREATED, Json(receipt)))
}

// ---------------------------------------------------------------------------
// Blocking bridge and error taxonomy
// ---------------------------------------------------------------------------

/// Every store call crosses this. See the module doc, property 3.
async fn blocking<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(operation)
        .await
        .map_err(|_| HttpError::storage(anyhow!("blocking task failed")))?
        .map_err(HttpError::from_store)
}

#[derive(Debug)]
pub(crate) struct HttpError {
    status: StatusCode,
    body: ErrorResponse,
}

impl HttpError {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self {
            status,
            body: ErrorResponse {
                code: code.to_string(),
                // Bounded: a store error can carry a channel id or a record
                // fragment, and an unbounded echo is how a wire error becomes
                // an information leak.
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
        tracing::warn!(error = %error, "conversation-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "conversation-source storage is unavailable",
        )
    }

    /// Map a leaf-contract violation onto the wire taxonomy.
    ///
    /// The same three actionable buckets the file lane uses: a version skew
    /// needs a satellite deploy, a limit breach needs a policy change, and
    /// everything else is a bug in what the producer sent.
    fn from_contract(error: ConversationSourceError) -> Self {
        match error {
            ConversationSourceError::UnsupportedSchema(_)
            | ConversationSourceError::ConversationPolicyMismatch { .. } => Self::unprocessable(
                "unsupported_contract",
                format!("conversation-source contract version is unsupported: {error}"),
            ),
            ConversationSourceError::TooManyRecords { .. }
            | ConversationSourceError::TooMuchText { .. } => Self::too_large(
                "limit_exceeded",
                format!("conversation-source input exceeds an enforced limit: {error}"),
            ),
            other => Self::unprocessable(
                "invalid_conversation_source_input",
                format!("conversation-source input violates the ingest contract: {other}"),
            ),
        }
    }

    /// Map an `anyhow` store error onto the wire taxonomy.
    ///
    /// I/O is the store being unavailable, a typed contract error in the chain
    /// is the producer's fault, and anything else is treated as unavailable
    /// rather than guessed at: reporting an unrecognized internal failure as a
    /// 4xx would tell a producer to change a request that was fine.
    fn from_store(error: anyhow::Error) -> Self {
        if error
            .chain()
            .any(|cause| cause.downcast_ref::<std::io::Error>().is_some())
        {
            return Self::storage(error);
        }
        if let Some(contract) = error
            .chain()
            .find_map(|cause| cause.downcast_ref::<ConversationSourceError>())
        {
            return Self::from_contract(contract.clone());
        }
        let rendered = error.to_string();
        if is_producer_correctable(&rendered) {
            return Self::unprocessable("invalid_ingest_state", rendered);
        }
        Self::storage(error)
    }
}

/// Store refusals a producer can fix by sending something different.
///
/// String matching is not a taxonomy and this is a compromise, named as one,
/// exactly as the file lane names its own. The default is the SAFE direction
/// (503, retry) so a miss here degrades to a retry rather than to a producer
/// permanently convinced its input was bad. Two refusals deliberately do NOT
/// match, because the producer can do nothing about either and must not be
/// told to change its request: a corrupt journal, and a corrupt workspace
/// binding file.
///
/// Each marker is a WHOLE store sentence rather than a keyword. A bare
/// `"workspace"` looked equivalent and was not: the binding lives in
/// `workspace.json`, so a truncated or hand-edited binding produces the
/// decode context `"decoding .../workspace.json"`, which carries a
/// `serde_json::Error` rather than an `io::Error` and therefore reaches this
/// function. The keyword matched it and reported server-side corruption to
/// the producer as its own fault.
fn is_producer_correctable(rendered: &str) -> bool {
    const MARKERS: [&str; 5] = [
        "invalid batch",
        "invalid revisions request",
        "batch workspace does not match",
        "has no workspace binding",
        "already bound to a different workspace",
    ];
    MARKERS.iter().any(|marker| rendered.contains(marker))
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        CatalogSnapshotV2, ConnectorKind, ConnectorSourceId, CorpusProject, ProjectId, ProjectScope,
    };
    use std::collections::BTreeSet;
    use std::io::Write as _;
    use std::path::Path as StdPath;

    /// Granted on the conversation lane and present in the catalog.
    const SOURCE_A: &str = "csrc_5f2c1d9a4b6e470e";
    /// Granted on the conversation lane and deliberately absent from the
    /// catalog, so it is pending onboarding.
    const SOURCE_B: &str = "csrc_00000000deadbeef";
    /// Granted on the FILE lane, so it must never reach these routes.
    const SOURCE_C: &str = "csrc_1111111111111111";

    fn token_file(root: &StdPath, name: &str, value: &str) -> std::path::PathBuf {
        let path = root.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(value.as_bytes()).unwrap();
        drop(file);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        path
    }

    fn scope(connector_source_id: &str) -> ConnectorScope {
        ConnectorScope::try_new(connector_source_id, "fixture").unwrap()
    }

    fn grant(
        connector_source_id: &str,
        profile: crate::config::ConnectorProfile,
    ) -> crate::config::ConnectorScopeGrant {
        crate::config::ConnectorScopeGrant {
            connector_source_id: ConnectorSourceId::parse(connector_source_id).unwrap(),
            connector_kind: ConnectorKind::parse("fixture").unwrap(),
            remote_authority: "workspace.example".to_string(),
            profile,
        }
    }

    fn catalog_with(connector_source_id: &str) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        let project_id = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id,
                scope: ProjectScope::Connector(scope(connector_source_id)),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Fixture Workspace".into(),
                created_at: "2026-08-13T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        catalog.sync_version();
        catalog.validate().unwrap();
        catalog
    }

    /// `producer-a` holds SOURCE_A (conversation, cataloged) and SOURCE_B
    /// (conversation, pending); `producer-files` holds SOURCE_C on the file
    /// lane.
    fn table(root: &StdPath) -> ConnectorGrantRuntime {
        use crate::config::ConnectorProfile;
        let config = crate::config::SourceConnectorsConfig {
            retained_conversations: Vec::new(),
            enabled: true,
            producers: vec![
                crate::config::ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: token_file(root, "token-a", &"a".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![
                        grant(SOURCE_A, ConnectorProfile::Conversation),
                        grant(SOURCE_B, ConnectorProfile::Conversation),
                    ],
                },
                crate::config::ConnectorProducerConfig {
                    producer_id: "producer-files".into(),
                    token_file: token_file(root, "token-b", &"b".repeat(64)),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_C, ConnectorProfile::File)],
                },
            ],
        };
        ConnectorGrantRuntime::build(&config, Some(&catalog_with(SOURCE_A))).unwrap()
    }

    fn caller(producer_id: &str) -> ConnectorGrant {
        ConnectorGrant {
            producer_id: producer_id.to_string(),
        }
    }

    #[test]
    fn a_granted_conversation_scope_is_authorized_only_for_the_producer_that_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let table = table(&root);

        require_ingest_scope(&table, &caller("producer-a"), &scope(SOURCE_A))
            .expect("the granting producer lands into its own scope");

        // The scope is real and cataloged; the CALLER is wrong. This is the
        // check that stops one producer landing into another's project, and it
        // runs before any durable write.
        let error = require_ingest_scope(&table, &caller("producer-files"), &scope(SOURCE_A))
            .expect_err("a scope granted to another producer is forbidden");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "scope_forbidden");

        // A scope nobody granted at all.
        let error = require_ingest_scope(
            &table,
            &caller("producer-a"),
            &ConnectorScope::try_new("csrc_2222222222222222", "fixture").unwrap(),
        )
        .expect_err("an ungranted scope is forbidden");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(error.body.code, "scope_forbidden");
    }

    #[test]
    fn a_file_lane_grant_cannot_land_conversation_records() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let table = table(&root);

        let error = require_ingest_scope(&table, &caller("producer-files"), &scope(SOURCE_C))
            .expect_err("a file grant opens no conversation lane");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
        assert_eq!(
            error.body.code, "scope_wrong_lane",
            "distinct from scope_forbidden: the grant is real, the endpoint is wrong"
        );

        // And the same refusal guards onboarding, which is otherwise the most
        // permissive route on this lane.
        let error = require_granted_lane(&table, &caller("producer-files"), &scope(SOURCE_C))
            .expect_err("a file grant may not onboard a conversation source either");
        assert_eq!(error.body.code, "scope_wrong_lane");
    }

    #[test]
    fn a_pending_onboarding_scope_is_refused_by_every_ingest_route() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let table = table(&root);
        assert!(table.is_pending_onboard(&scope(SOURCE_B)));

        let error = require_ingest_scope(&table, &caller("producer-a"), &scope(SOURCE_B))
            .expect_err("a pending scope opens no ingest lane");
        assert_eq!(error.status, StatusCode::CONFLICT);
        assert_eq!(error.body.code, "scope_pending_onboarding");

        // The same scope IS eligible for onboarding: that admission is the
        // whole reason "add the scope to the daemon config first" works.
        require_granted_lane(&table, &caller("producer-a"), &scope(SOURCE_B))
            .expect("onboarding admits a granted, uncataloged conversation scope");
    }

    #[test]
    fn a_disabled_connector_family_authorizes_nothing() {
        let table = ConnectorGrantRuntime::disabled();
        assert!(!table.enabled());
        let error = require_ingest_scope(&table, &caller("producer-a"), &scope(SOURCE_A))
            .expect_err("a disabled family holds no grants");
        assert_eq!(error.status, StatusCode::FORBIDDEN);
    }

    #[test]
    fn a_scope_selector_needs_both_halves_and_is_parsed_not_trusted() {
        let query = ScopeQuery {
            connector_source_id: SOURCE_A.into(),
            connector_kind: "fixture".into(),
        };
        assert_eq!(query.scope().unwrap(), scope(SOURCE_A));

        // A path-shaped id never becomes a scope: the durable id type validates
        // and the route reports it as a contract error rather than a 403.
        let bad = ScopeQuery {
            connector_source_id: "../drive-ops".into(),
            connector_kind: "fixture".into(),
        };
        let error = bad.scope().expect_err("a malformed selector is refused");
        assert_eq!(error.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(error.body.code, "invalid_conversation_source_input");
    }

    #[test]
    fn contract_violations_map_onto_the_three_actionable_buckets() {
        let skew = HttpError::from_contract(ConversationSourceError::ConversationPolicyMismatch {
            received: "old".into(),
            expected: bbox_conversation_source::CONVERSATION_POLICY_VERSION,
        });
        assert_eq!(skew.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(skew.body.code, "unsupported_contract");

        let limit = HttpError::from_contract(ConversationSourceError::TooManyRecords {
            actual: 10_000,
            limit: 500,
        });
        assert_eq!(limit.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(limit.body.code, "limit_exceeded");

        let shape = HttpError::from_contract(ConversationSourceError::BatchDigestMismatch);
        assert_eq!(shape.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(shape.body.code, "invalid_conversation_source_input");
    }

    #[test]
    fn a_tampered_identity_refusal_reaches_the_producer_as_its_own_fault() {
        // The store's workspace refusals are the durable half of the tamper
        // check, and a producer CAN fix them by sending the right workspace, so
        // they must not render as an opaque 503.
        let mismatch = HttpError::from_store(anyhow!(
            "batch workspace does not match the workspace this connector scope is bound to"
        ));
        assert_eq!(mismatch.status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(mismatch.body.code, "invalid_ingest_state");

        let unbound = HttpError::from_store(anyhow!(
            "connector scope has no workspace binding; onboard it before landing records"
        ));
        assert_eq!(unbound.status, StatusCode::UNPROCESSABLE_ENTITY);
    }

    #[test]
    fn an_io_failure_and_a_corrupt_journal_are_both_unavailable() {
        let io = HttpError::from_store(anyhow::Error::new(std::io::Error::other("disk gone")));
        assert_eq!(io.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(io.body.code, "storage_unavailable");

        // Journal corruption is real and serious, and the producer can do
        // nothing about it. Telling it to change its request would be a lie.
        let corrupt = HttpError::from_store(anyhow!(
            "an unparseable line in the middle of a conversation journal is corruption, not a \
             torn append"
        ));
        assert_eq!(corrupt.status, StatusCode::SERVICE_UNAVAILABLE);

        // The near miss that a keyword marker got wrong: a corrupt workspace
        // BINDING renders a decode context naming workspace.json, and it is
        // server-side damage, not a request the producer can fix.
        let corrupt_binding = HttpError::from_store(anyhow!(
            "decoding /state/conversation-sources/scopes/abc/workspace.json"
        ));
        assert_eq!(
            corrupt_binding.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "a corrupt binding file is not the producer's fault"
        );

        let unknown = HttpError::from_store(anyhow!("something nobody classified"));
        assert_eq!(unknown.status, StatusCode::SERVICE_UNAVAILABLE);
    }

    // -----------------------------------------------------------------
    // End to end, through the route layer
    // -----------------------------------------------------------------
    //
    // The unit tests above prove the authorization helpers in isolation.
    // These drive the real router with a real bearer, which is the only way
    // to catch the failures that live BETWEEN the pieces: middleware that
    // never admits the token, an onboard that mints a catalog project without
    // binding a workspace, a promotion path that rebuilds producer auth from a
    // config the fixture never installed.
    //
    // Mirrors `file_source_activation::tests::onboarding_mints_the_connector_
    // project_and_is_idempotent`, including its fixture discipline: the grant
    // is installed into `state.config` AND the live auth table together,
    // because the onboard route promotes a freshly minted scope by REBUILDING
    // auth from `state.config`. A fixture that installs only the table leaves
    // the daemon self-inconsistent and the next request dies in the
    // middleware's enabled() gate with a 503 that looks nothing like the
    // fixture bug it is.

    /// Install one conversation-lane producer grant into both the config and
    /// the live auth table; returns the bearer secret.
    fn install_conversation_connectors(
        state: &Arc<SharedState>,
        root: &StdPath,
        catalog: Option<&CatalogSnapshotV2>,
    ) -> String {
        let token_secret = "c".repeat(64);
        let config = crate::config::SourceConnectorsConfig {
            retained_conversations: Vec::new(),
            enabled: true,
            producers: vec![crate::config::ConnectorProducerConfig {
                producer_id: "producer-a".into(),
                token_file: token_file(root, "token-e2e", &token_secret),
                token_files: Vec::new(),
                scopes: vec![grant(
                    SOURCE_A,
                    crate::config::ConnectorProfile::Conversation,
                )],
            }],
        };
        state.config.write().source_connectors = config.clone();
        let connectors = ConnectorGrantRuntime::build(&config, catalog).unwrap();
        state
            .code_sources
            .install_auth_for_test(std::sync::Arc::new(
                crate::server::producer_auth::ProducerAuthRuntime::for_test_connectors(Arc::new(
                    connectors,
                )),
            ));
        token_secret
    }

    /// Two producers: `producer-a` holds SOURCE_A on the conversation lane,
    /// `producer-files` holds SOURCE_C on the file lane. Returns the
    /// conversation bearer and the OTHER producer's bearer, so a test can
    /// authenticate as a producer that does not hold the scope it asks about.
    fn install_two_producer_connectors(
        state: &Arc<SharedState>,
        root: &StdPath,
    ) -> (String, String) {
        let conversation_secret = "c".repeat(64);
        let other_secret = "d".repeat(64);
        let config = crate::config::SourceConnectorsConfig {
            retained_conversations: Vec::new(),
            enabled: true,
            producers: vec![
                crate::config::ConnectorProducerConfig {
                    producer_id: "producer-a".into(),
                    token_file: token_file(root, "token-a", &conversation_secret),
                    token_files: Vec::new(),
                    scopes: vec![grant(
                        SOURCE_A,
                        crate::config::ConnectorProfile::Conversation,
                    )],
                },
                crate::config::ConnectorProducerConfig {
                    producer_id: "producer-files".into(),
                    token_file: token_file(root, "token-other", &other_secret),
                    token_files: Vec::new(),
                    scopes: vec![grant(SOURCE_C, crate::config::ConnectorProfile::File)],
                },
            ],
        };
        state.config.write().source_connectors = config.clone();
        // The runtime refuses to build without catalog authority; the empty
        // catalog `pending_state` already initialized is exactly the table
        // this fixture wants (membership fails before any catalog lookup).
        let pinned = state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap();
        let connectors = ConnectorGrantRuntime::build(&config, Some(pinned.catalog())).unwrap();
        state
            .code_sources
            .install_auth_for_test(std::sync::Arc::new(
                crate::server::producer_auth::ProducerAuthRuntime::for_test_connectors(Arc::new(
                    connectors,
                )),
            ));
        (conversation_secret, other_secret)
    }

    /// A catalog-backed daemon whose only producer authority is a granted but
    /// uncataloged conversation scope: the one state the onboard route admits
    /// and every ingest route refuses.
    fn pending_state(root: &StdPath) -> (Arc<SharedState>, String) {
        let catalog_root = root.join("catalog");
        std::fs::create_dir_all(&catalog_root).unwrap();
        let catalog_projects_path = catalog_root.join("projects.json");
        bbox_indexing::project_catalog_store::ProjectCatalogStore::initialize_empty(
            &catalog_projects_path,
        )
        .unwrap();
        let state = Arc::new(SharedState::for_test_catalog(root, &catalog_projects_path));
        let store = state.project_authority.catalog_store().unwrap().clone();
        let pinned = store.snapshot().unwrap();
        let token = install_conversation_connectors(&state, root, Some(pinned.catalog()));
        assert!(
            state
                .code_sources
                .producer_auth()
                .connectors()
                .is_pending_onboard(&scope(SOURCE_A)),
            "a granted conversation scope with no catalog project is pending onboarding"
        );
        (state, token)
    }

    fn http_request(
        method: &str,
        uri: &str,
        token: &str,
        body: axum::body::Body,
        user_agent: &str,
    ) -> axum::http::Request<axum::body::Body> {
        axum::http::Request::builder()
            .method(method)
            .uri(uri)
            .header(axum::http::header::CONTENT_TYPE, "application/json")
            .header(axum::http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(axum::http::header::USER_AGENT, user_agent)
            .body(body)
            .unwrap()
    }

    async fn json_body<T: serde::de::DeserializeOwned>(response: Response) -> T {
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn onboard_request() -> ConversationCatalogOnboardRequestV1 {
        ConversationCatalogOnboardRequestV1 {
            schema_version: bbox_conversation_source::CATALOG_ONBOARD_SCHEMA_VERSION,
            scope: scope(SOURCE_A),
            probed_connector_kind: "fixture".into(),
            probed_remote_authority: "workspace.example".into(),
            probed_workspace_id: "T0FIXTURE".into(),
            probed_workspace_display_name: Some("Fixture Workspace".into()),
            display_name: "Fixture Workspace".into(),
        }
    }

    /// One landable batch, digest included, for the pending-scope refusal.
    fn one_message_batch() -> ConversationBatchV1 {
        let records = vec![bbox_conversation_source::ConversationMessageRecordV1 {
            channel_id: "C0OPSFIX".into(),
            message_ts: "1712345678.000200".into(),
            revision: 0,
            author_id: "U0HUMAN".into(),
            author_kind: bbox_conversation_source::AuthorKindV1::Human,
            thread_parent_ts: None,
            subtype: None,
            text: "the deploy pipeline wedged on the lock table".into(),
            edited_ts: None,
            reactions: Vec::new(),
            attachments: Vec::new(),
            observed_at: "2026-08-13T00:00:00Z".into(),
        }];
        ConversationBatchV1 {
            schema_version: bbox_conversation_source::SCHEMA_VERSION,
            conversation_policy_version: bbox_conversation_source::CONVERSATION_POLICY_VERSION
                .to_string(),
            scope: scope(SOURCE_A),
            workspace_id: "T0FIXTURE".into(),
            channel_id: "C0OPSFIX".into(),
            batch_digest: bbox_conversation_source::batch_digest(&records),
            records,
            observed_at: "2026-08-13T00:00:00Z".into(),
        }
    }

    #[tokio::test]
    async fn retained_conversation_history_survives_grant_removal_without_authorizing_writes() {
        use bbox_corpus_index::transcripts::adapters::{
            TranscriptReadAdapter, TranscriptScanTarget,
        };
        use bbox_corpus_index::transcripts::conversation::ConversationTranscriptAdapter;
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, token) = pending_state(&root);
        let app = router(state.clone()).with_state(state.clone());
        for (endpoint, body) in [
            (
                "catalog/onboard",
                serde_json::to_vec(&onboard_request()).unwrap(),
            ),
            ("batches", serde_json::to_vec(&one_message_batch()).unwrap()),
        ] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "POST",
                    &format!("/internal/conversation-source/v1/{endpoint}"),
                    &token,
                    axum::body::Body::from(body),
                    "fixture",
                ))
                .await
                .unwrap();
            assert!(
                response.status().is_success(),
                "initial ingest failed: {}",
                response.status()
            );
            if endpoint == "catalog/onboard" {
                // The test records provider is a fixed snapshot. Refresh the
                // auth table from the newly written catalog as production does.
                let pinned = state
                    .project_authority
                    .catalog_store()
                    .unwrap()
                    .snapshot()
                    .unwrap();
                install_conversation_connectors(&state, &root, Some(pinned.catalog()));
            }
        }
        let catalog = state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap();
        let landing_root = root.join(CONVERSATION_SOURCE_DIR);
        let live = super::super::conversation_enrollment::resolve(
            &state.config.read().source_connectors,
            Some(catalog.catalog()),
            &landing_root,
        )
        .unwrap();
        let retained = crate::config::SourceConnectorsConfig {
            retained_conversations: vec![crate::config::RetainedConversationSource {
                connector_source_id: scope(SOURCE_A).connector_source_id().clone(),
                connector_kind: scope(SOURCE_A).connector_kind().clone(),
                workspace_id: "T0FIXTURE".into(),
                remote_authority: "workspace.example".into(),
            }],
            ..Default::default()
        };
        let read_enrollments = super::super::conversation_enrollment::resolve(
            &retained,
            Some(catalog.catalog()),
            &landing_root,
        )
        .unwrap();
        assert_eq!(
            read_enrollments, live,
            "retention preserves the exact existing transcript projection identity"
        );
        let auth = ConnectorGrantRuntime::build(&retained, Some(catalog.catalog())).unwrap();
        assert!(auth.authenticate(&token).is_none());
        assert!(auth.grants().is_empty());
        assert_eq!(
            require_ingest_scope(&auth, &caller("producer-a"), &scope(SOURCE_A))
                .unwrap_err()
                .body
                .code,
            "scope_forbidden"
        );
        state.config.write().source_connectors = retained.clone();
        state.code_sources.install_auth_for_test(Arc::new(
            crate::server::producer_auth::ProducerAuthRuntime::for_test_connectors(Arc::new(auth)),
        ));

        let store = state.conversation_sources.store();
        let journal = store.channel_journal_path(&scope(SOURCE_A), "T0FIXTURE", "C0OPSFIX");
        let before = std::fs::read(&journal).unwrap();
        for endpoint in ["catalog/onboard", "channels", "batches", "revisions"] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "POST",
                    &format!("/internal/conversation-source/v1/{endpoint}"),
                    &token,
                    axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                    "fixture",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let error: ErrorResponse = json_body(response).await;
            assert_eq!(
                error.code, "service_disabled",
                "retained rows cannot reopen the ingest middleware"
            );
        }
        // Retention also stays read-only while an unrelated producer keeps
        // the ingest family enabled: neither the retired token nor a valid
        // token for a different source can write this history.
        let other_token = "d".repeat(64);
        let mut enabled_retention = retained.clone();
        enabled_retention.enabled = true;
        enabled_retention.producers = vec![crate::config::ConnectorProducerConfig {
            producer_id: "producer-other".into(),
            token_file: token_file(&root, "token-retention-other", &other_token),
            token_files: Vec::new(),
            scopes: vec![grant(SOURCE_C, crate::config::ConnectorProfile::File)],
        }];
        let auth =
            ConnectorGrantRuntime::build(&enabled_retention, Some(catalog.catalog())).unwrap();
        assert!(auth.authenticate(&token).is_none());
        state.config.write().source_connectors = enabled_retention;
        state.code_sources.install_auth_for_test(Arc::new(
            crate::server::producer_auth::ProducerAuthRuntime::for_test_connectors(Arc::new(auth)),
        ));
        for (bearer, expected_status, expected_code) in [
            (&token, StatusCode::UNAUTHORIZED, "unauthorized"),
            (&other_token, StatusCode::FORBIDDEN, "scope_forbidden"),
        ] {
            let response = app
                .clone()
                .oneshot(http_request(
                    "POST",
                    "/internal/conversation-source/v1/batches",
                    bearer,
                    axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                    "fixture",
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), expected_status);
            assert_eq!(
                json_body::<ErrorResponse>(response).await.code,
                expected_code
            );
        }
        assert_eq!(std::fs::read(&journal).unwrap(), before);
        let adapter = ConversationTranscriptAdapter::open(&landing_root, read_enrollments).unwrap();
        let locations = adapter
            .scan_locations(TranscriptScanTarget::Sessions)
            .unwrap();
        assert_eq!(locations.len(), 1);
        let snapshot = adapter.load_snapshot(&locations[0]).unwrap();
        assert_eq!(snapshot.events.len(), 1);
        assert_eq!(
            snapshot.events[0].content,
            one_message_batch().records[0].text
        );
        assert_eq!(
            snapshot.events[0].timestamp.as_deref(),
            Some("2024-04-05T19:34:38Z")
        );

        // Retention does not override an existing membership revocation.
        store
            .record_roster(
                &scope(SOURCE_A),
                "T0FIXTURE",
                &[bbox_conversation_source::ChannelObservationV1 {
                    channel_id: "C0OPSFIX".into(),
                    observed_name: None,
                    class: bbox_conversation_source::ChannelClassV1::Public,
                    is_member: false,
                    observed_at: "2026-08-13T00:10:00Z".into(),
                }],
                false,
                "2026-08-13T00:10:00Z",
            )
            .unwrap();
        assert!(
            adapter
                .scan_locations(TranscriptScanTarget::Sessions)
                .unwrap()
                .is_empty()
        );
        let revoked = super::super::conversation_enrollment::resolve(
            &crate::config::SourceConnectorsConfig::default(),
            Some(catalog.catalog()),
            &landing_root,
        )
        .unwrap();
        assert!(ConversationTranscriptAdapter::open(&landing_root, revoked).is_none());
    }

    #[tokio::test]
    async fn onboarding_mints_the_conversation_project_binds_the_workspace_and_is_idempotent() {
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, token) = pending_state(&root);
        let app = router(state.clone()).with_state(state.clone());

        // A pending scope opens NO ingest lane. This refusal is what makes
        // "add the scope to the daemon config first" possible without opening
        // a lane for a project that does not exist.
        let response = app
            .clone()
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/batches",
                &token,
                axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            StatusCode::CONFLICT,
            "landing under a pending scope is a conflict, deliberately distinct \
             from a forbidden one: they are different fixes"
        );
        let refusal: ErrorResponse = json_body(response).await;
        assert_eq!(refusal.code, "scope_pending_onboarding");

        let response = app
            .clone()
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/catalog/onboard",
                &token,
                axum::body::Body::from(serde_json::to_vec(&onboard_request()).unwrap()),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let receipt: ConversationCatalogOnboardResponseV1 = json_body(response).await;
        assert!(receipt.created_project, "the first onboard mints it");

        // The catalog owns the scope-to-project mapping.
        let store = state.project_authority.catalog_store().unwrap().clone();
        let pinned = store.snapshot().unwrap();
        let project = pinned
            .catalog()
            .projects
            .values()
            .find(|project| {
                matches!(&project.scope, ProjectScope::Connector(owned) if owned == &scope(SOURCE_A))
            })
            .expect("onboarding records a connector-scoped project");
        assert_eq!(project.project_id.as_str(), receipt.project_id);

        // And the workspace binding exists, which is the durable half of the
        // tamper check every later batch is measured against. It must be
        // written by onboarding, or the first batch is refused as unbound.
        let binding = state
            .conversation_sources
            .store()
            .workspace_binding(&scope(SOURCE_A))
            .unwrap()
            .expect("onboarding binds the probed workspace");
        assert_eq!(binding.workspace_id, "T0FIXTURE");

        // Find-or-create: a producer driving onboard before every cycle is
        // safe to retry rather than duplicating the project.
        let response = app
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/catalog/onboard",
                &token,
                axum::body::Body::from(serde_json::to_vec(&onboard_request()).unwrap()),
                "",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        let replayed: ConversationCatalogOnboardResponseV1 = json_body(response).await;
        assert_eq!(replayed.project_id, receipt.project_id);
        assert!(
            !replayed.created_project,
            "only the call that MINTED the project reports creating it"
        );
        assert_eq!(
            store.snapshot().unwrap().catalog().projects.len(),
            1,
            "a retried onboard is not a second project"
        );
        assert_eq!(
            state
                .conversation_sources
                .store()
                .workspace_binding(&scope(SOURCE_A))
                .unwrap()
                .map(|binding| binding.workspace_id),
            Some("T0FIXTURE".to_string()),
            "a retried onboard rebinds the same workspace rather than a second one"
        );
    }

    /// Presence is boot-scoped memory fed by the INGEST verbs only: a batch
    /// records the User-Agent its process actually sent, and the status read
    /// renders that contact verbatim without refreshing it, so staleness is
    /// observable and no unrelated poller can hide a silent satellite.
    #[tokio::test]
    async fn an_authenticated_contact_is_visible_on_the_status_surface() {
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, token) = pending_state(&root);
        let app = router(state.clone()).with_state(state.clone());
        const ONBOARD_AGENT: &str = "onboard-probe/1";
        const BATCH_AGENT: &str = "bbox-slack-collector/9.9.9-distinct";
        const STATUS_AGENT: &str = "an-unrelated-poller/0";

        let response = app
            .clone()
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/catalog/onboard",
                &token,
                axum::body::Body::from(serde_json::to_vec(&onboard_request()).unwrap()),
                ONBOARD_AGENT,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert!(
            state
                .conversation_sources
                .producer_contact(&scope(SOURCE_A))
                .is_none(),
            "onboard is not an ingest verb; it records no contact"
        );

        // Stand in for the daemon's post-onboard promotion by rebuilding the
        // auth table against the catalog the onboard just wrote: the fixture's
        // records provider does not observe the new project, so without this
        // the scope would stay pending and every ingest route would refuse it.
        let pinned = state
            .project_authority
            .catalog_store()
            .unwrap()
            .snapshot()
            .unwrap();
        install_conversation_connectors(&state, &root, Some(pinned.catalog()));
        let app = router(state.clone()).with_state(state.clone());

        // The batch is the contact that matters: it is the call a live
        // satellite makes every cycle.
        let response = app
            .clone()
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/batches",
                &token,
                axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                BATCH_AGENT,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        // BEFORE any status read: the map itself holds the batch's contact,
        // with the User-Agent that process sent.
        let recorded = state
            .conversation_sources
            .producer_contact(&scope(SOURCE_A))
            .expect("an authenticated batch records a contact");
        assert_eq!(recorded.user_agent, BATCH_AGENT);

        // A DIFFERENT caller polls status...
        let response = app
            .oneshot(http_request(
                "GET",
                &format!(
                    "/internal/conversation-source/v1/status?connector_source_id={SOURCE_A}\
                     &connector_kind=fixture"
                ),
                &token,
                axum::body::Body::empty(),
                STATUS_AGENT,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let status: ConversationStatusResponseV1 = json_body(response).await;
        let producer = status
            .producer
            .expect("an authenticated contact renders on the status surface");
        // ...and is shown the BATCH's contact, never its own header.
        assert_eq!(producer.user_agent, BATCH_AGENT);
        assert_eq!(
            producer.last_seen_at,
            contact_rfc3339(recorded.last_seen_epoch_secs)
        );
        // The poll refreshed nothing: the map still holds the batch's stamp.
        let after = state
            .conversation_sources
            .producer_contact(&scope(SOURCE_A))
            .expect("the status read does not clear presence");
        assert_eq!(after.user_agent, BATCH_AGENT);
        assert_eq!(after.last_seen_epoch_secs, recorded.last_seen_epoch_secs);
    }

    /// The map only ever names scopes an authorized ingest reached. A request
    /// that fails the bearer check records nothing.
    #[tokio::test]
    async fn an_unauthenticated_request_records_no_contact() {
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, _token) = pending_state(&root);
        let app = router(state.clone()).with_state(state.clone());

        let response = app
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/batches",
                "a-bearer-nobody-installed",
                axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                "ghost/1",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(
            state
                .conversation_sources
                .producer_contact(&scope(SOURCE_A))
                .is_none(),
            "a request that fails the bearer check is not a contact"
        );
    }

    /// Same rule for a VALID bearer held by the wrong producer: the grant
    /// check runs before the map is touched, so a scope another producer
    /// asked about never appears.
    #[tokio::test]
    async fn a_scope_the_bearer_does_not_hold_records_no_contact() {
        use tower::ServiceExt as _;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (state, _token) = pending_state(&root);
        let (_, other_token) = install_two_producer_connectors(&state, &root);
        let app = router(state.clone()).with_state(state.clone());

        let response = app
            .oneshot(http_request(
                "POST",
                "/internal/conversation-source/v1/batches",
                &other_token,
                axum::body::Body::from(serde_json::to_vec(&one_message_batch()).unwrap()),
                "wrong-producer/1",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert!(
            state
                .conversation_sources
                .producer_contact(&scope(SOURCE_A))
                .is_none(),
            "a scope the bearer does not hold is not a contact"
        );
    }

    #[test]
    fn the_wire_error_body_is_bounded() {
        let error = HttpError::new(StatusCode::BAD_REQUEST, "code", "x".repeat(4096));
        assert_eq!(
            error.body.message.chars().count(),
            512,
            "an unbounded echo of store context is how a wire error becomes a leak"
        );
    }

    #[test]
    fn a_hostile_user_agent_is_truncated_before_it_is_stored() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::USER_AGENT,
            axum::http::HeaderValue::from_str(&"x".repeat(10_000)).unwrap(),
        );
        let stored = user_agent(&headers);
        assert_eq!(
            stored.chars().count(),
            256,
            "the presence map must stay bounded against hostile headers"
        );
    }
}

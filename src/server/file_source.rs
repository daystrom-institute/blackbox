//! The `/internal/file-source/v1/*` publication route layer.
//!
//! This is the file-lane sibling of [`super::code_source`]'s router and it is
//! deliberately shaped the same way: one authenticated middleware in front of
//! the whole family, one handler per wire verb, and a single error taxonomy
//! that renders [`bbox_file_source::ErrorResponse`]. Route separation exists so
//! producer grants, limits, and health can be reasoned about per lane, not
//! because the transport differs.
//!
//! Four properties of this layer are load-bearing and are the reason it is not
//! simply a thinner copy:
//!
//! 1. **Authorization is by connector scope, never by path.** Every handler
//!    reaches a scope before it reaches the store, and the scope comes from
//!    either the request body (begin, onboard) or the durable upload record
//!    (everything keyed on `upload_id`). A producer therefore cannot name
//!    another producer's upload id and have the store answer: the record's own
//!    `producer_id` and scope are checked against the authenticated grant.
//!
//! 2. **The pending-onboarding admission is exclusive to the onboard route.**
//!    A configured scope with no catalog project is admitted there and refused
//!    everywhere else, which is the code-collection precedent unchanged. That
//!    is what makes "add the scope to the daemon config first" possible without
//!    opening a publication lane for a project that does not exist.
//!
//! 3. **Nothing blocks on the request path.** [`FileSourceStore`] is
//!    synchronous by design (fsync, atomic rename, parent fsync), so every call
//!    into it crosses [`blocking`], which is `spawn_blocking`. This is not
//!    stylistic: `clippy.toml`'s disallowed-methods gate and
//!    `scripts/lint-concurrency.sh` both police it, and the healthz starvation
//!    this week turned on exactly this rule. Any disk-lock wait on a request
//!    path belongs on the blocking pool.
//!
//! 4. **Blob bytes are bounded before they are read, twice.** The route layer
//!    caps the body, then requires an exact `Content-Length` that matches the
//!    size the manifest declared for that hash, and only then hands bytes to
//!    the store, which re-verifies the digest. A producer that authenticates
//!    still does not get to decide what the CAS holds.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use axum::Router;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, Extension, Json, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use bbox_corpus_core::project_catalog::ConnectorScope;
use bbox_file_source::{
    BeginFileUploadRequestV1, BeginFileUploadResponseV1, ErrorResponse,
    FileCatalogOnboardRequestV1, FileCatalogOnboardResponseV1, FileGenerationStatusV1,
    FileManifestPageV1, FileSourceError, FinalizeFileUploadResponseV1, MissingFileBlobsPageV1,
};
use bbox_file_source_store::{FileSourceStore, MISSING_BLOBS_PAGE, UploadRecord};
use serde::Deserialize;

use super::SharedState;
use super::connector_grants::ConnectorGrantRuntime;
use super::producer_auth::ConnectorGrant;

/// The state-dir subdirectory this lane owns. Sibling of `code-sources`,
/// separate because the generation model, the key, and the retention window
/// are all this lane's own.
const FILE_SOURCE_DIR: &str = "file-sources";

/// Body ceiling for one blob PUT. The per-path cap is finer (the chunker
/// registry claims different ceilings per extension and
/// [`bbox_code_source::max_bytes_for_path`] is the authority), but a router
/// layer has to bound the body before any path is known, so this is the
/// coarsest honest bound: the largest a single document may ever be.
const MAX_BLOB_BODY_BYTES: usize = bbox_code_source::MAX_DOCUMENT_FILE_BYTES as usize;

/// Runtime holder for the connector generation store.
///
/// Deliberately thin next to `CodeSourceRuntime`: that type also owns producer
/// auth, cutback reconciliation, transition guards, and a retirement
/// coordinator. This lane's authorization lives in the connector grant table
/// (rebuilt with `ProducerAuthRuntime`, so there is exactly one place grants
/// are resolved), and phase 1 has no cutback lane at all, so a struct with one
/// field is the honest shape rather than a placeholder for machinery that does
/// not exist.
pub(crate) struct FileSourceRuntime {
    store: Arc<FileSourceStore>,
}

impl FileSourceRuntime {
    pub(crate) fn open(config: &crate::config::Config) -> Result<Self> {
        let store = FileSourceStore::open(config.paths.state_dir.join(FILE_SOURCE_DIR))?;
        Ok(Self {
            store: Arc::new(store),
        })
    }

    pub(crate) fn store(&self) -> Arc<FileSourceStore> {
        self.store.clone()
    }

    #[cfg(test)]
    pub(crate) fn for_test(root: &std::path::Path) -> Self {
        Self {
            store: Arc::new(FileSourceStore::open(root.join(FILE_SOURCE_DIR)).unwrap()),
        }
    }
}

pub(crate) fn router(state: Arc<SharedState>) -> Router<Arc<SharedState>> {
    Router::new()
        .route(
            "/internal/file-source/v1/catalog/onboard",
            post(catalog_onboard).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/file-source/v1/uploads",
            post(begin_upload).layer(DefaultBodyLimit::max(64 * 1024)),
        )
        .route(
            "/internal/file-source/v1/uploads/{upload_id}/manifest/{page}",
            put(put_manifest_page).layer(DefaultBodyLimit::max(
                bbox_file_source::MAX_MANIFEST_PAGE_BYTES,
            )),
        )
        .route(
            "/internal/file-source/v1/uploads/{upload_id}/manifest/complete",
            post(complete_manifest).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/file-source/v1/uploads/{upload_id}/missing",
            get(missing_blobs),
        )
        .route(
            "/internal/file-source/v1/uploads/{upload_id}/blobs/{hash}",
            put(put_blob).layer(DefaultBodyLimit::max(MAX_BLOB_BODY_BYTES)),
        )
        .route(
            "/internal/file-source/v1/uploads/{upload_id}/finalize",
            post(finalize_upload).layer(DefaultBodyLimit::max(1)),
        )
        .route(
            "/internal/file-source/v1/generations/{generation}/status",
            get(generation_status),
        )
        .route_layer(axum::middleware::from_fn_with_state(
            state,
            super::producer_auth::authenticate_file_source_request,
        ))
}

// ---------------------------------------------------------------------------
// Authorization helpers
// ---------------------------------------------------------------------------

/// The grant table this request authenticates against.
fn connectors(state: &SharedState) -> Arc<ConnectorGrantRuntime> {
    state.code_sources.producer_auth().connectors().clone()
}

/// Require that this producer holds a PUBLICATION grant on the scope.
///
/// Deliberately stricter than the onboard route: a scope that is granted but
/// not yet cataloged is pending onboarding, and admitting it here would open a
/// publication lane for a project that does not exist. The refusal names the
/// pending case separately so an operator reading a 403 can tell "you never
/// granted this" from "you granted it but never onboarded it", which are two
/// completely different fixes.
fn require_publication_scope(
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
    if connectors.is_pending_onboard(scope) {
        return Err(HttpError::new(
            StatusCode::CONFLICT,
            "scope_pending_onboarding",
            "connector scope has no catalog project yet; onboard it before publishing",
        ));
    }
    Ok(())
}

/// Load an upload and prove it belongs to this producer.
///
/// The record is the authority on both facts. Checking the grant against the
/// SCOPE alone would be insufficient: two producers cannot share a scope today,
/// but an upload id is a derived, guessable-length token and the record's own
/// `producer_id` is the only thing that makes "this upload is yours" a
/// statement about identity rather than about configuration.
async fn authorized_upload(
    state: &SharedState,
    grant: &ConnectorGrant,
    upload_id: &str,
) -> Result<UploadRecord, HttpError> {
    let store = state.file_sources.store();
    let wanted = upload_id.to_string();
    let record = blocking(move || store.load_upload(&wanted)).await?;
    let record = record
        .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "not_found", "unknown upload"))?;
    if record.producer_id != grant.producer_id {
        // Deliberately NOT_FOUND rather than FORBIDDEN: a producer that names
        // another producer's upload id learns only that it has no such upload,
        // never that the id exists.
        return Err(HttpError::new(
            StatusCode::NOT_FOUND,
            "not_found",
            "unknown upload",
        ));
    }
    require_publication_scope(&connectors(state), grant, &record.scope)?;
    Ok(record)
}

/// Resolve which of this producer's granted scopes holds a generation.
///
/// The store is keyed on scope and the status route carries only a generation
/// id, so the scope has to come from somewhere. Searching the producer's OWN
/// grants is what makes this authorization-safe by construction: a generation
/// under a scope this producer was never granted is simply not found, with no
/// separate permission check to forget.
async fn scope_for_generation(
    state: &SharedState,
    grant: &ConnectorGrant,
    generation_id: &str,
) -> Result<(ConnectorScope, FileGenerationStatusV1), HttpError> {
    let scopes: Vec<ConnectorScope> = connectors(state)
        .grants()
        .iter()
        .filter(|expectation| expectation.producer_id == grant.producer_id)
        .map(|expectation| expectation.scope.clone())
        .collect();
    let store = state.file_sources.store();
    let wanted = generation_id.to_string();
    let found = blocking(move || {
        for scope in scopes {
            if let Some(status) = store.status(&scope, &wanted)? {
                return Ok(Some((scope, status)));
            }
        }
        Ok(None)
    })
    .await?;
    found.ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "not_found", "unknown generation"))
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn begin_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<BeginFileUploadRequestV1>,
) -> Result<impl IntoResponse, HttpError> {
    // Header validation FIRST, before the grant check touches the scope: a
    // malformed scope is a contract error, not an authorization one, and
    // reporting it as 403 would send an operator hunting a grant that is fine.
    request
        .validate_header()
        .map_err(HttpError::from_contract)?;
    require_publication_scope(&connectors(&state), &grant, &request.descriptor.scope)?;

    let store = state.file_sources.store();
    let producer_id = grant.producer_id.clone();
    let record = blocking(move || {
        store.begin_upload(
            &producer_id,
            &request.descriptor,
            &request.telemetry,
            request.degradation.as_ref(),
        )
    })
    .await?;
    Ok((
        StatusCode::CREATED,
        Json(BeginFileUploadResponseV1 {
            upload_id: record.upload_id,
            ordinal: record.ordinal,
            max_page_entries: bbox_file_source::MAX_MANIFEST_PAGE_ENTRIES,
            max_page_bytes: bbox_file_source::MAX_MANIFEST_PAGE_BYTES,
        }),
    ))
}

async fn put_manifest_page(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path((upload_id, page)): Path<(String, u64)>,
    Json(body): Json<FileManifestPageV1>,
) -> Result<StatusCode, HttpError> {
    authorized_upload(&state, &grant, &upload_id).await?;
    if body.entries.len() > bbox_file_source::MAX_MANIFEST_PAGE_ENTRIES {
        return Err(HttpError::too_large(
            "limit_exceeded",
            "manifest page carries more entries than the negotiated maximum",
        ));
    }
    let store = state.file_sources.store();
    blocking(move || store.append_manifest_page(&upload_id, page, body.entries)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn complete_manifest(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path(upload_id): Path<String>,
) -> Result<Json<MissingFileBlobsPageV1>, HttpError> {
    let record = authorized_upload(&state, &grant, &upload_id).await?;
    let store = state.file_sources.store();
    let hashes = blocking(move || store.complete_manifest(&upload_id)).await?;
    Ok(Json(page_of(record.generation_id, hashes, None)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MissingQuery {
    cursor: Option<String>,
}

async fn missing_blobs(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path(upload_id): Path<String>,
    Query(query): Query<MissingQuery>,
) -> Result<Json<MissingFileBlobsPageV1>, HttpError> {
    let record = authorized_upload(&state, &grant, &upload_id).await?;
    let store = state.file_sources.store();
    // Recomputed from the CAS by the store rather than replayed from a
    // remembered list, which is what makes a restart mid-publication resume
    // instead of re-requesting blobs that landed before the crash.
    let hashes = blocking(move || store.missing_blobs(&upload_id)).await?;
    Ok(Json(page_of(
        record.generation_id,
        hashes,
        query.cursor.as_deref(),
    )))
}

/// Page a sorted missing-hash list from an opaque cursor.
///
/// The cursor is the last hash of the previous page and paging is a strict
/// `>` scan, so a page boundary is stable even though the underlying set
/// SHRINKS between calls as blobs land. A numeric offset would skip hashes
/// whenever a concurrent upload shortened the list, which is precisely the
/// resume case this endpoint exists to serve.
fn page_of(
    generation_id: String,
    hashes: Vec<String>,
    cursor: Option<&str>,
) -> MissingFileBlobsPageV1 {
    let mut page: Vec<String> = hashes
        .into_iter()
        .filter(|hash| cursor.is_none_or(|cursor| hash.as_str() > cursor))
        .collect();
    let next_cursor = if page.len() > MISSING_BLOBS_PAGE {
        page.truncate(MISSING_BLOBS_PAGE);
        page.last().cloned()
    } else {
        None
    };
    MissingFileBlobsPageV1 {
        generation_id,
        hashes: page,
        next_cursor,
    }
}

async fn put_blob(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path((upload_id, hash)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Result<StatusCode, HttpError> {
    let record = authorized_upload(&state, &grant, &upload_id).await?;
    bbox_file_source::validate_sha256(&hash).map_err(HttpError::from_contract)?;

    // The manifest is the authority on what this upload may carry. Without
    // this check an authenticated producer could push arbitrary bytes into a
    // shared, content-addressed blob store under any digest it liked; the
    // store would verify the digest and happily keep them.
    let expected_size = record
        .entries()
        .iter()
        .find(|entry| entry.content_sha256 == hash)
        .map(|entry| entry.size)
        .ok_or_else(|| {
            HttpError::unprocessable(
                "blob_not_in_manifest",
                "this upload's manifest declares no entry with that content hash",
            )
        })?;

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

    // Buffered rather than streamed to a temporary file, unlike the code lane.
    // That lane streams because its store installs a blob FROM A PATH; this
    // store's `install_blob` takes bytes, so staging to disk first would add a
    // write and a read back for nothing. The body is bounded twice before it
    // is read: by the route's `DefaultBodyLimit` and by the exact
    // `Content-Length` match above.
    let bytes = axum::body::to_bytes(body, MAX_BLOB_BODY_BYTES)
        .await
        .map_err(|_| {
            HttpError::too_large("limit_exceeded", "blob body exceeds the lane ceiling")
        })?;
    if bytes.len() as u64 != expected_size {
        return Err(HttpError::unprocessable(
            "blob_size_mismatch",
            "received body length does not match the manifest",
        ));
    }

    let store = state.file_sources.store();
    blocking(move || store.install_blob(&hash, &bytes)).await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn finalize_upload(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path(upload_id): Path<String>,
) -> Result<Json<FinalizeFileUploadResponseV1>, HttpError> {
    authorized_upload(&state, &grant, &upload_id).await?;
    let store = state.file_sources.store();
    let generation = blocking(move || store.finalize(&upload_id)).await?;
    let generation_id = generation.generation_id.clone();
    // Activation runs on its own lane rather than inside this handler.
    // Finalize means "the bytes are durable and the manifest agrees with its
    // descriptor"; staging into the index is a separate, longer transaction
    // whose failure must not be reported as a failed upload. A producer polls
    // the status route to learn the outcome, which is the same contract the
    // code lane offers and the reason `FileGenerationStateV1::Superseded`
    // counts as terminal SUCCESS.
    //
    // The generation therefore rests at `Ready` when this route layer is the
    // only thing mounted. Wiring the activation lane onto this seam is the
    // next commit; until it lands, a finalized generation is durable, correct,
    // and simply not yet searchable.
    let _ = &generation;
    Ok(Json(FinalizeFileUploadResponseV1 {
        status_url: format!("/internal/file-source/v1/generations/{generation_id}/status"),
        generation_id,
    }))
}

async fn generation_status(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Path(generation): Path<String>,
) -> Result<Json<FileGenerationStatusV1>, HttpError> {
    let (_scope, status) = scope_for_generation(&state, &grant, &generation).await?;
    Ok(Json(status))
}

async fn catalog_onboard(
    State(state): State<Arc<SharedState>>,
    Extension(grant): Extension<ConnectorGrant>,
    Json(request): Json<FileCatalogOnboardRequestV1>,
) -> Result<impl IntoResponse, HttpError> {
    request.validate().map_err(HttpError::from_contract)?;

    // The pending-onboarding admission, and the ONLY place it applies. A
    // configured scope with no catalog project reaches the composite here and
    // nowhere else; every publication handler routes through
    // `require_publication_scope`, which refuses it.
    let connectors = connectors(&state);
    let granted = connectors.grants().iter().any(|expectation| {
        expectation.producer_id == grant.producer_id && expectation.scope == request.scope
    });
    if !granted {
        return Err(HttpError::new(
            StatusCode::FORBIDDEN,
            "scope_forbidden",
            "connector scope is not authorized for this producer",
        ));
    }

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
    let receipt = blocking(move || -> Result<FileCatalogOnboardResponseV1> {
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let onboard = bbox_indexing::project_catalog_admin::ConnectorOnboardRequest {
            producer_id,
            scope: request.scope.clone(),
            probed_connector_kind: request.probed_connector_kind.clone(),
            probed_remote_authority: request.probed_remote_authority.clone(),
            probed_remote_root_id: request.probed_remote_root_id.clone(),
            probed_remote_display_name: request.probed_remote_display_name.clone(),
            display_name: request.display_name.clone(),
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
        Ok(FileCatalogOnboardResponseV1 {
            project_id: receipt.project_id.as_str().to_string(),
            created_project: receipt.created,
            epoch: receipt.catalog_epoch,
        })
    })
    .await?;

    // Promote the freshly cataloged scope out of `pending_onboard` so the
    // publication routes stop refusing it. The connector grant table is
    // rebuilt as part of the producer-auth snapshot, and `build` resolves
    // connectors BEFORE the code-collection disabled early return, so this is
    // correct even on a daemon that runs connectors with code collection off.
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
                // Bounded: a store error can carry a path or a manifest
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
        tracing::warn!(error = %error, "file-source storage operation failed");
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "storage_unavailable",
            "file-source storage is unavailable",
        )
    }

    /// Map a leaf-contract violation onto the wire taxonomy.
    ///
    /// The three buckets are the ones a producer can act on differently:
    /// a version skew needs a satellite deploy, a limit breach needs a policy
    /// change, and everything else is a bug in what the producer sent.
    fn from_contract(error: FileSourceError) -> Self {
        match error {
            FileSourceError::UnsupportedSchema(_)
            | FileSourceError::ConnectorPolicyMismatch { .. } => Self::unprocessable(
                "unsupported_contract",
                format!("file-source contract version is unsupported: {error}"),
            ),
            FileSourceError::FileTooLarge { .. }
            | FileSourceError::TooManyFiles { .. }
            | FileSourceError::TooManyBytes { .. } => Self::too_large(
                "limit_exceeded",
                format!("file-source input exceeds an enforced limit: {error}"),
            ),
            other => Self::unprocessable(
                "invalid_file_source_input",
                format!("file-source input violates the publication contract: {other}"),
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
            .find_map(|cause| cause.downcast_ref::<FileSourceError>())
        {
            return Self::from_contract(contract.clone());
        }
        // The store reports its own invariant breaches as plain `anyhow`
        // messages (a replayed upload with a different descriptor, a manifest
        // that disagrees with its descriptor, a finalize with blobs still
        // missing). Those are all producer-correctable, so they render as 422
        // with the store's own words rather than as an opaque 503.
        let rendered = error.to_string();
        if is_producer_correctable(&rendered) {
            return Self::unprocessable("invalid_upload_state", rendered);
        }
        Self::storage(error)
    }
}

/// Store refusals a producer can fix by sending something different.
///
/// String matching is not a taxonomy and this is a compromise, named as one:
/// the store speaks `anyhow` and phase 1 does not restructure it into a typed
/// error enum, because that store is brand new and a sibling lane is still
/// landing against the shape. The default is the SAFE direction (503, retry)
/// so a miss here degrades to a retry rather than to a producer permanently
/// convinced its input was bad.
fn is_producer_correctable(rendered: &str) -> bool {
    const MARKERS: [&str; 6] = [
        "different descriptor",
        "manifest rejected",
        "already completed its manifest",
        "has not completed its manifest",
        "is missing",
        "invalid manifest entry",
    ];
    MARKERS.iter().any(|marker| rendered.contains(marker))
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

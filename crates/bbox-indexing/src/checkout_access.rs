//! Validated checkout access and bounded operational observations.
//!
//! Checkout paths are host authority, not corpus identity. This module keeps
//! that authority behind a broker: callers name a logical project and an
//! attachment selector, an injected authority adapter resolves the current
//! attachment state, and the broker independently checks identity, scope,
//! capability, intent, path containment, and the conservative path gate before
//! returning a lease. The broker deliberately has no dependency on the legacy
//! `ProjectRecord` shape.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const OBSERVATION_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 256;
const LIFECYCLE_MUTATION_BIT: usize = 1usize << (usize::BITS - 1);

/// Closed set of operations permitted to obtain checkout filesystem authority.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutAccessKind {
    LocalProjectWalk,
    GitHistory,
    PublisherConfigTreeRead,
    KnowledgeGapOverlayRead,
    Blame,
    RenderFileProvider,
    ProvenanceNoteIo,
    ArtifactWatchDiscovery,
    RepositoryMutation,
}

impl CheckoutAccessKind {
    pub const ALL: [Self; 9] = [
        Self::LocalProjectWalk,
        Self::GitHistory,
        Self::PublisherConfigTreeRead,
        Self::KnowledgeGapOverlayRead,
        Self::Blame,
        Self::RenderFileProvider,
        Self::ProvenanceNoteIo,
        Self::ArtifactWatchDiscovery,
        Self::RepositoryMutation,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::LocalProjectWalk => "local_project_walk",
            Self::GitHistory => "git_history",
            Self::PublisherConfigTreeRead => "publisher_config_tree_read",
            Self::KnowledgeGapOverlayRead => "knowledge_gap_overlay_read",
            Self::Blame => "blame",
            Self::RenderFileProvider => "render_file_provider",
            Self::ProvenanceNoteIo => "provenance_note_io",
            Self::ArtifactWatchDiscovery => "artifact_watch_discovery",
            Self::RepositoryMutation => "repository_mutation",
        }
    }

    const fn permits(self, intent: CheckoutAccessIntent) -> bool {
        match self {
            Self::RenderFileProvider | Self::ProvenanceNoteIo => true,
            Self::RepositoryMutation => matches!(intent, CheckoutAccessIntent::Write),
            _ => matches!(intent, CheckoutAccessIntent::Read),
        }
    }
}

/// Whether the requested operation may only inspect or may mutate a checkout.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutAccessIntent {
    Read,
    Write,
}

/// Bounded source label for retirement evidence. No path or project-derived
/// value may be added here.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutAccessSourceLane {
    NativeAttachment,
    LegacyProjectRecord,
    LegacyCheckoutRegistry,
    LegacyPathResolver,
}

impl CheckoutAccessSourceLane {
    pub const ALL: [Self; 4] = [
        Self::NativeAttachment,
        Self::LegacyProjectRecord,
        Self::LegacyCheckoutRegistry,
        Self::LegacyPathResolver,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeAttachment => "native_attachment",
            Self::LegacyProjectRecord => "legacy_project_record",
            Self::LegacyCheckoutRegistry => "legacy_checkout_registry",
            Self::LegacyPathResolver => "legacy_path_resolver",
        }
    }

    pub const fn is_compatibility(self) -> bool {
        matches!(
            self,
            Self::LegacyProjectRecord | Self::LegacyCheckoutRegistry | Self::LegacyPathResolver
        )
    }
}

/// Path-free selector used to choose one attachment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckoutAttachmentSelector {
    /// Use the operator-selected attachment. The authority adapter must reject
    /// this selector when there is no unique selection.
    Selected,
    AttachmentId(String),
    CheckoutId(String),
    /// Phase-0 compatibility selector. The raw value is confined to the
    /// authority request and is never recorded in observations or durable
    /// corpus state. Later catalog phases delete this lane.
    LegacyPath(String),
}

/// Complete request presented to the broker. `expected_scope = None` is valid
/// for a legacy local project with no durable published scope. The sole
/// discovery exception is a `PublisherConfigTreeRead` request using
/// `Selected` plus `LegacyProjectRecord`; its lease returns the validated
/// scope that subsequent capability-specific requests must match exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutAccessRequest {
    pub project_id: String,
    pub attachment: CheckoutAttachmentSelector,
    pub expected_scope: Option<PublishedScope>,
    pub kind: CheckoutAccessKind,
    pub intent: CheckoutAccessIntent,
    pub source_lane: CheckoutAccessSourceLane,
}

/// Lifecycle state returned by an authority adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckoutAttachmentStatus {
    Active,
    Detached,
    Unavailable,
}

/// Authority-owned candidate. It is not a lease and must never be passed to a
/// filesystem consumer. Only the broker can turn it into a lease.
#[derive(Debug, Clone)]
pub struct CheckoutAccessCandidate {
    pub project_id: String,
    pub attachment_id: String,
    pub checkout_id: String,
    pub published_scope: Option<PublishedScope>,
    pub branch_ref: Option<String>,
    pub checkout_root: PathBuf,
    pub project_root: PathBuf,
    pub status: CheckoutAttachmentStatus,
    pub capabilities: BTreeSet<CheckoutAccessKind>,
    pub lifetime_guard: Option<Arc<dyn CheckoutAccessLifetimeGuard>>,
}

/// Opaque mutation pin retained by write leases. It does not lock either
/// registry or block read-side work. Lifecycle writers fail fast while a pin
/// exists and may retry after the filesystem mutation completes.
pub trait CheckoutAccessLifetimeGuard: fmt::Debug + Send + Sync + 'static {}

#[derive(Debug)]
struct ActiveCheckoutMutation {
    state: Arc<AtomicUsize>,
}

impl CheckoutAccessLifetimeGuard for ActiveCheckoutMutation {}

#[derive(Debug)]
struct CombinedCheckoutAccessLifetimeGuards {
    _guards: Vec<Arc<dyn CheckoutAccessLifetimeGuard>>,
}

impl CheckoutAccessLifetimeGuard for CombinedCheckoutAccessLifetimeGuards {}

impl Drop for ActiveCheckoutMutation {
    fn drop(&mut self) {
        self.state.fetch_sub(1, Ordering::Release);
    }
}

/// Exclusive, fail-fast lifecycle token. Holding it prevents new write leases
/// while a project or checkout attachment record is changed.
#[derive(Debug)]
pub struct CheckoutLifecycleMutationGuard {
    state: Arc<AtomicUsize>,
}

impl Drop for CheckoutLifecycleMutationGuard {
    fn drop(&mut self) {
        self.state.store(0, Ordering::Release);
    }
}

/// Short-lived publication fence acquired only after filesystem/Git
/// preparation is complete. While it is alive, lifecycle mutations fail fast,
/// closing the gap between final lease revalidation and publication without
/// holding a registry lock across the expensive preparation phase.
#[derive(Debug)]
pub struct CheckoutPublicationGuard {
    _pin: Arc<dyn CheckoutAccessLifetimeGuard>,
}

/// Adapter over the current host-local attachment authority. Phase 0 adapters
/// may read compatibility stores; later catalog work can replace them without
/// changing callers or granting paths outside this boundary.
pub trait CheckoutAccessAuthority: Send + Sync + 'static {
    fn resolve(
        &self,
        request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError>;

    /// Re-run the conservative read/write path gate after both roots have been
    /// canonicalized and their containment relationship has been verified.
    fn revalidate_conservative_path_gate(
        &self,
        request: &CheckoutAccessRequest,
        candidate: &CheckoutAccessCandidate,
    ) -> std::result::Result<(), CheckoutAccessError>;
}

/// Deterministic remote-shaped probe. It never resolves or touches a path.
#[derive(Debug, Default)]
pub struct DenyCheckoutAccess;

impl CheckoutAccessAuthority for DenyCheckoutAccess {
    fn resolve(
        &self,
        _request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::DeniedByTestProbe,
            "checkout access denied by the injected test probe",
        ))
    }

    fn revalidate_conservative_path_gate(
        &self,
        _request: &CheckoutAccessRequest,
        _candidate: &CheckoutAccessCandidate,
    ) -> std::result::Result<(), CheckoutAccessError> {
        Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::DeniedByTestProbe,
            "checkout access denied by the injected test probe",
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutAccessErrorCode {
    InvalidRequest,
    AttachmentNotFound,
    AttachmentInactive,
    ProjectMismatch,
    SelectorMismatch,
    CheckoutIdentityMismatch,
    ScopeMismatch,
    CapabilityDenied,
    IntentDenied,
    ConservativePathGateDenied,
    InvalidRoot,
    UnsafeRelativePath,
    WriteIntentRequired,
    LifecycleBusy,
    DeniedByTestProbe,
    ObservationUnavailable,
}

impl CheckoutAccessErrorCode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::AttachmentNotFound => "attachment_not_found",
            Self::AttachmentInactive => "attachment_inactive",
            Self::ProjectMismatch => "project_mismatch",
            Self::SelectorMismatch => "selector_mismatch",
            Self::CheckoutIdentityMismatch => "checkout_identity_mismatch",
            Self::ScopeMismatch => "scope_mismatch",
            Self::CapabilityDenied => "capability_denied",
            Self::IntentDenied => "intent_denied",
            Self::ConservativePathGateDenied => "conservative_path_gate_denied",
            Self::InvalidRoot => "invalid_root",
            Self::UnsafeRelativePath => "unsafe_relative_path",
            Self::WriteIntentRequired => "write_intent_required",
            Self::LifecycleBusy => "lifecycle_busy",
            Self::DeniedByTestProbe => "denied_by_test_probe",
            Self::ObservationUnavailable => "observation_unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckoutAccessError {
    pub code: CheckoutAccessErrorCode,
    pub diagnostic: String,
}

impl CheckoutAccessError {
    pub fn new(code: CheckoutAccessErrorCode, diagnostic: impl Into<String>) -> Self {
        Self {
            code,
            diagnostic: diagnostic.into(),
        }
    }
}

impl fmt::Display for CheckoutAccessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.diagnostic)
    }
}

impl std::error::Error for CheckoutAccessError {}

/// Capability token returned only after all broker checks and the successful
/// observation commit. It exposes roots through getters and relative-path
/// resolvers, never by reconstructing them from corpus metadata.
#[derive(Debug)]
pub struct ValidatedCheckoutLease {
    project_id: String,
    attachment_id: String,
    checkout_id: String,
    published_scope: Option<PublishedScope>,
    branch_ref: Option<String>,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
    source_lane: CheckoutAccessSourceLane,
    checkout_root: PathBuf,
    project_root: PathBuf,
    checkout_root_handle: File,
    project_root_handle: File,
    _lifetime_guard: Option<Arc<dyn CheckoutAccessLifetimeGuard>>,
    acquisition_sequence: u64,
    attachment_selector: CheckoutAttachmentSelector,
    expected_scope: Option<PublishedScope>,
    request_project_id: String,
}

impl ValidatedCheckoutLease {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn attachment_id(&self) -> &str {
        &self.attachment_id
    }

    pub fn checkout_id(&self) -> &str {
        &self.checkout_id
    }

    pub fn published_scope(&self) -> Option<&PublishedScope> {
        self.published_scope.as_ref()
    }

    pub fn branch_ref(&self) -> Option<&str> {
        self.branch_ref.as_deref()
    }

    pub fn kind(&self) -> CheckoutAccessKind {
        self.kind
    }

    pub fn intent(&self) -> CheckoutAccessIntent {
        self.intent
    }

    pub fn source_lane(&self) -> CheckoutAccessSourceLane {
        self.source_lane
    }

    pub fn checkout_root(&self) -> &Path {
        &self.checkout_root
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    pub fn acquisition_sequence(&self) -> u64 {
        self.acquisition_sequence
    }

    /// Read one regular file relative to the leased project root without
    /// following symlinks in any relative component. The directory handle is
    /// captured when the lease is acquired, closing the provider's former
    /// canonicalize-then-open race.
    pub fn read_relative_file(
        &self,
        relative: &Path,
    ) -> std::result::Result<(PathBuf, Vec<u8>), CheckoutAccessError> {
        let relative = validate_relative_path(relative)?;
        let bytes = read_file_beneath(&self.project_root_handle, relative)?;
        Ok((self.project_root.join(relative), bytes))
    }

    /// Capture every top-level JSON file in one relative directory through
    /// the leased project-root descriptor. Directory replacement, symlinks,
    /// entry churn, non-UTF-8 JSON names, and non-regular JSON entries fail
    /// closed. A missing directory is the sole empty-snapshot case.
    pub fn read_relative_json_directory(
        &self,
        relative: impl AsRef<Path>,
    ) -> std::result::Result<BTreeMap<String, Vec<u8>>, CheckoutAccessError> {
        let relative = validate_relative_path(relative.as_ref())?;
        read_json_directory_beneath(&self.project_root_handle, relative)
    }

    /// Check one relative regular-file marker through the retained project
    /// root descriptor. Missing is `false`; symlinks and non-regular entries
    /// fail closed.
    pub fn relative_regular_file_exists(
        &self,
        relative: impl AsRef<Path>,
    ) -> std::result::Result<bool, CheckoutAccessError> {
        let relative = validate_relative_path(relative.as_ref())?;
        regular_file_exists_beneath(&self.project_root_handle, relative)
    }

    /// Checkout-root counterpart used for host-local transaction markers that
    /// live above a nested project root.
    pub fn checkout_relative_regular_file_exists(
        &self,
        relative: impl AsRef<Path>,
    ) -> std::result::Result<bool, CheckoutAccessError> {
        let relative = validate_relative_path(relative.as_ref())?;
        regular_file_exists_beneath(&self.checkout_root_handle, relative)
    }

    /// Resolve an existing relative path and reject lexical or symlink escape.
    pub fn resolve_existing(
        &self,
        relative: impl AsRef<Path>,
    ) -> std::result::Result<PathBuf, CheckoutAccessError> {
        let relative = validate_relative_path(relative.as_ref())?;
        let resolved = std::fs::canonicalize(self.project_root.join(relative)).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative path does not resolve inside the leased project root",
            )
        })?;
        if !resolved.starts_with(&self.project_root) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative path escapes the leased project root",
            ));
        }
        Ok(resolved)
    }

    /// Resolve a write target whose parent already exists. Canonicalizing the
    /// parent rejects symlink escape while still allowing a new final entry.
    pub fn resolve_write_target(
        &self,
        relative: impl AsRef<Path>,
    ) -> std::result::Result<PathBuf, CheckoutAccessError> {
        if self.intent != CheckoutAccessIntent::Write {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::WriteIntentRequired,
                "the lease does not grant write intent",
            ));
        }
        let relative = validate_relative_path(relative.as_ref())?;
        let file_name = relative.file_name().ok_or_else(|| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "write target must name a final path component",
            )
        })?;
        let relative_parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent =
            std::fs::canonicalize(self.project_root.join(relative_parent)).map_err(|_| {
                CheckoutAccessError::new(
                    CheckoutAccessErrorCode::UnsafeRelativePath,
                    "write target parent does not resolve inside the leased project root",
                )
            })?;
        if !parent.starts_with(&self.project_root) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "write target parent escapes the leased project root",
            ));
        }
        let target = parent.join(file_name);
        if std::fs::symlink_metadata(&target).is_ok() {
            let resolved = std::fs::canonicalize(&target).map_err(|_| {
                CheckoutAccessError::new(
                    CheckoutAccessErrorCode::UnsafeRelativePath,
                    "existing write target cannot be safely resolved",
                )
            })?;
            if !resolved.starts_with(&self.project_root) {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::UnsafeRelativePath,
                    "existing write target escapes the leased project root",
                ));
            }
            return Ok(resolved);
        }
        Ok(target)
    }
}

fn validate_relative_path(path: &Path) -> std::result::Result<&Path, CheckoutAccessError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::UnsafeRelativePath,
            "path must be a non-empty relative path without parent traversal",
        ));
    }
    Ok(path)
}

/// Central broker. The authority can change across the migration while the
/// validation and instrumentation contract remains fixed.
pub struct CheckoutAccessBroker {
    authority: Arc<dyn CheckoutAccessAuthority>,
    observations: CheckoutAccessObservations,
    lifecycle_state: Arc<AtomicUsize>,
}

impl CheckoutAccessBroker {
    pub fn new(
        authority: Arc<dyn CheckoutAccessAuthority>,
        observations: CheckoutAccessObservations,
    ) -> Self {
        Self {
            authority,
            observations,
            lifecycle_state: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn acquire(
        &self,
        request: CheckoutAccessRequest,
    ) -> std::result::Result<ValidatedCheckoutLease, CheckoutAccessError> {
        let mutation_pin = match request.intent {
            CheckoutAccessIntent::Read => None,
            CheckoutAccessIntent::Write => match self.acquire_mutation_pin() {
                Ok(pin) => Some(pin),
                Err(error) => {
                    self.observations
                        .record(
                            request.kind,
                            request.source_lane,
                            CheckoutAccessOutcome::Denied,
                        )
                        .map_err(observation_error)?;
                    return Err(error);
                }
            },
        };
        let result = self.acquire_unobserved(&request).map(|mut candidate| {
            candidate.lifetime_guard = match (candidate.lifetime_guard.take(), mutation_pin) {
                (Some(authority), Some(mutation)) => {
                    Some(Arc::new(CombinedCheckoutAccessLifetimeGuards {
                        _guards: vec![authority, mutation],
                    }))
                }
                (authority, mutation) => authority.or(mutation),
            };
            candidate
        });
        self.finish_acquire(request, result)
    }

    fn acquire_mutation_pin(
        &self,
    ) -> std::result::Result<Arc<dyn CheckoutAccessLifetimeGuard>, CheckoutAccessError> {
        loop {
            let current = self.lifecycle_state.load(Ordering::Acquire);
            if current & LIFECYCLE_MUTATION_BIT != 0 {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::LifecycleBusy,
                    "checkout lifecycle mutation is in progress",
                ));
            }
            let active = current & !LIFECYCLE_MUTATION_BIT;
            if active == LIFECYCLE_MUTATION_BIT - 1 {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::LifecycleBusy,
                    "checkout mutation pin capacity is exhausted",
                ));
            }
            if self
                .lifecycle_state
                .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return Ok(Arc::new(ActiveCheckoutMutation {
                    state: self.lifecycle_state.clone(),
                }));
            }
        }
    }

    /// Acquire exclusive lifecycle authority without waiting for active
    /// filesystem mutations. Callers retry or return the typed busy result.
    pub fn lifecycle_mutation_guard(
        &self,
    ) -> std::result::Result<CheckoutLifecycleMutationGuard, CheckoutAccessError> {
        self.lifecycle_state
            .compare_exchange(
                0,
                LIFECYCLE_MUTATION_BIT,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| CheckoutLifecycleMutationGuard {
                state: self.lifecycle_state.clone(),
            })
            .map_err(|_| {
                CheckoutAccessError::new(
                    CheckoutAccessErrorCode::LifecycleBusy,
                    "checkout filesystem mutation is in progress",
                )
            })
    }

    fn finish_acquire(
        &self,
        request: CheckoutAccessRequest,
        result: std::result::Result<CheckoutAccessCandidate, CheckoutAccessError>,
    ) -> std::result::Result<ValidatedCheckoutLease, CheckoutAccessError> {
        match result {
            Ok(candidate) => {
                let checkout_root_handle = match File::open(&candidate.checkout_root) {
                    Ok(handle) => handle,
                    Err(_) => {
                        self.observations
                            .record(
                                request.kind,
                                request.source_lane,
                                CheckoutAccessOutcome::Denied,
                            )
                            .map_err(observation_error)?;
                        return Err(CheckoutAccessError::new(
                            CheckoutAccessErrorCode::InvalidRoot,
                            "validated checkout root could not be opened",
                        ));
                    }
                };
                let project_root_handle = match File::open(&candidate.project_root) {
                    Ok(handle) => handle,
                    Err(_) => {
                        self.observations
                            .record(
                                request.kind,
                                request.source_lane,
                                CheckoutAccessOutcome::Denied,
                            )
                            .map_err(observation_error)?;
                        return Err(CheckoutAccessError::new(
                            CheckoutAccessErrorCode::InvalidRoot,
                            "validated project root could not be opened",
                        ));
                    }
                };
                let sequence = self
                    .observations
                    .record(
                        request.kind,
                        request.source_lane,
                        CheckoutAccessOutcome::Granted,
                    )
                    .map_err(observation_error)?;
                Ok(ValidatedCheckoutLease {
                    project_id: candidate.project_id,
                    attachment_id: candidate.attachment_id,
                    checkout_id: candidate.checkout_id,
                    published_scope: candidate.published_scope,
                    branch_ref: candidate.branch_ref,
                    kind: request.kind,
                    intent: request.intent,
                    source_lane: request.source_lane,
                    checkout_root: candidate.checkout_root,
                    project_root: candidate.project_root,
                    checkout_root_handle,
                    project_root_handle,
                    _lifetime_guard: candidate.lifetime_guard,
                    acquisition_sequence: sequence,
                    attachment_selector: request.attachment,
                    expected_scope: request.expected_scope,
                    request_project_id: request.project_id,
                })
            }
            Err(error) => {
                self.observations
                    .record(
                        request.kind,
                        request.source_lane,
                        CheckoutAccessOutcome::Denied,
                    )
                    .map_err(observation_error)?;
                Err(error)
            }
        }
    }

    pub fn health(&self) -> CheckoutAccessHealth {
        self.observations.health()
    }

    /// Revalidate a lease immediately before a filesystem-derived result is
    /// published. This closes detach, scope-change, and selector races without
    /// treating the lease's cached roots as durable authority.
    pub fn revalidate(
        &self,
        lease: &ValidatedCheckoutLease,
    ) -> std::result::Result<(), CheckoutAccessError> {
        let request = CheckoutAccessRequest {
            project_id: lease.request_project_id.clone(),
            attachment: lease.attachment_selector.clone(),
            expected_scope: lease.expected_scope.clone(),
            kind: lease.kind,
            intent: lease.intent,
            source_lane: lease.source_lane,
        };
        let candidate = self.acquire_unobserved(&request)?;
        if candidate.project_id != lease.project_id
            || candidate.attachment_id != lease.attachment_id
            || candidate.checkout_id != lease.checkout_id
            || candidate.published_scope != lease.published_scope
            || candidate.branch_ref != lease.branch_ref
            || candidate.checkout_root != lease.checkout_root
            || candidate.project_root != lease.project_root
        {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::ConservativePathGateDenied,
                "checkout authority changed after lease acquisition",
            ));
        }
        Ok(())
    }

    /// Fence one final publication boundary. The lifecycle pin is acquired
    /// before revalidation and retained by the returned guard, so an attachment
    /// detach or relocation can happen either before this call (and invalidate
    /// it) or after the caller drops the guard, never between validation and
    /// publication.
    pub fn publication_guard(
        &self,
        lease: &ValidatedCheckoutLease,
    ) -> std::result::Result<CheckoutPublicationGuard, CheckoutAccessError> {
        self.publication_guard_for(std::iter::once(lease))
    }

    /// Multi-lease form for one atomic derived publication. Every contributing
    /// lease is revalidated while the same lifecycle pin is held.
    pub fn publication_guard_for<'a>(
        &self,
        leases: impl IntoIterator<Item = &'a ValidatedCheckoutLease>,
    ) -> std::result::Result<CheckoutPublicationGuard, CheckoutAccessError> {
        let pin = self.acquire_mutation_pin()?;
        let mut count = 0usize;
        for lease in leases {
            self.revalidate(lease)?;
            count += 1;
        }
        if count == 0 {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::InvalidRequest,
                "publication requires at least one checkout lease",
            ));
        }
        Ok(CheckoutPublicationGuard { _pin: pin })
    }

    fn acquire_unobserved(
        &self,
        request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        validate_request(request)?;
        let candidate = self.authority.resolve(request)?;
        self.validate_candidate_unobserved(request, candidate)
    }

    fn validate_candidate_unobserved(
        &self,
        request: &CheckoutAccessRequest,
        mut candidate: CheckoutAccessCandidate,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        validate_id("candidate project_id", &candidate.project_id)?;
        validate_id("candidate attachment_id", &candidate.attachment_id)?;
        validate_id("candidate checkout_id", &candidate.checkout_id)?;
        if !request.project_id.is_empty() && candidate.project_id != request.project_id {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::ProjectMismatch,
                "the resolved attachment belongs to a different project",
            ));
        }
        match &request.attachment {
            CheckoutAttachmentSelector::Selected => {}
            CheckoutAttachmentSelector::AttachmentId(expected)
                if candidate.attachment_id != *expected =>
            {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::SelectorMismatch,
                    "the resolved attachment does not match the requested attachment id",
                ));
            }
            CheckoutAttachmentSelector::CheckoutId(expected)
                if candidate.checkout_id != *expected =>
            {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::CheckoutIdentityMismatch,
                    "the resolved attachment does not match the requested checkout id",
                ));
            }
            CheckoutAttachmentSelector::LegacyPath(_) => {}
            _ => {}
        }
        if candidate.status != CheckoutAttachmentStatus::Active {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::AttachmentInactive,
                "the resolved attachment is not active",
            ));
        }
        let scope_discovery = (request.kind == CheckoutAccessKind::PublisherConfigTreeRead
            && request.expected_scope.is_none()
            && request.source_lane == CheckoutAccessSourceLane::LegacyProjectRecord
            && matches!(&request.attachment, CheckoutAttachmentSelector::Selected))
            || (request.expected_scope.is_none()
                && request.source_lane == CheckoutAccessSourceLane::LegacyPathResolver
                && matches!(
                    &request.attachment,
                    CheckoutAttachmentSelector::LegacyPath(_)
                ));
        if !scope_discovery && candidate.published_scope != request.expected_scope {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::ScopeMismatch,
                "the resolved attachment scope does not match the requested project scope",
            ));
        }
        if !candidate.capabilities.contains(&request.kind) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::CapabilityDenied,
                "the resolved attachment does not grant the requested capability",
            ));
        }

        let checkout_root = canonical_directory(&candidate.checkout_root)?;
        let project_root = canonical_directory(&candidate.project_root)?;
        if !project_root.starts_with(&checkout_root) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::InvalidRoot,
                "the project root is outside the checkout root",
            ));
        }
        candidate.checkout_root = checkout_root;
        candidate.project_root = project_root;
        self.authority
            .revalidate_conservative_path_gate(request, &candidate)?;
        Ok(candidate)
    }
}

fn validate_request(
    request: &CheckoutAccessRequest,
) -> std::result::Result<(), CheckoutAccessError> {
    match &request.attachment {
        CheckoutAttachmentSelector::LegacyPath(value) => {
            if !request.project_id.is_empty() {
                validate_id("project_id", &request.project_id)?;
            }
            if value.trim().is_empty() || value.len() > 4096 || value.contains('\0') {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::InvalidRequest,
                    "legacy path selector must be a non-empty bounded value",
                ));
            }
        }
        CheckoutAttachmentSelector::Selected => {
            validate_id("project_id", &request.project_id)?;
        }
        CheckoutAttachmentSelector::AttachmentId(value) => {
            validate_id("project_id", &request.project_id)?;
            validate_id("attachment_id", value)?;
        }
        CheckoutAttachmentSelector::CheckoutId(value) => {
            validate_id("project_id", &request.project_id)?;
            validate_id("checkout_id", value)?;
        }
    }
    if !request.kind.permits(request.intent) {
        return Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::IntentDenied,
            "the requested access kind does not permit this intent",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn read_file_beneath(
    root: &File,
    relative: &Path,
) -> std::result::Result<Vec<u8>, CheckoutAccessError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let components = relative.components().collect::<Vec<_>>();
    let mut directory = root.try_clone().map_err(file_read_error)?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout path contains an unsafe component",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout path contains a NUL byte",
            )
        })?;
        let is_last = index + 1 == components.len();
        let flags = if is_last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        // SAFETY: `directory` owns a live directory fd, `name` is NUL
        // terminated, and a successful fd is immediately wrapped in `File`.
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(file_read_error(std::io::Error::last_os_error()));
        }
        // SAFETY: `openat` returned a new owned descriptor.
        let mut opened = unsafe { File::from_raw_fd(fd) };
        if is_last {
            let metadata = opened.metadata().map_err(file_read_error)?;
            if !metadata.is_file() {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::UnsafeRelativePath,
                    "relative checkout path does not name a regular file",
                ));
            }
            let mut bytes = Vec::new();
            opened.read_to_end(&mut bytes).map_err(file_read_error)?;
            return Ok(bytes);
        }
        directory = opened;
    }
    Err(CheckoutAccessError::new(
        CheckoutAccessErrorCode::UnsafeRelativePath,
        "relative checkout path is empty",
    ))
}

#[cfg(not(unix))]
fn read_file_beneath(
    _root: &File,
    _relative: &Path,
) -> std::result::Result<Vec<u8>, CheckoutAccessError> {
    Err(CheckoutAccessError::new(
        CheckoutAccessErrorCode::ConservativePathGateDenied,
        "descriptor-relative checkout reads are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn regular_file_exists_beneath(
    root: &File,
    relative: &Path,
) -> std::result::Result<bool, CheckoutAccessError> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let components = relative.components().collect::<Vec<_>>();
    let mut directory = root.try_clone().map_err(file_read_error)?;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout marker contains an unsafe component",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout marker contains a NUL byte",
            )
        })?;
        let is_last = index + 1 == components.len();
        let flags = if is_last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(false);
            }
            return Err(file_read_error(error));
        }
        let opened = unsafe { File::from_raw_fd(fd) };
        if is_last {
            if !opened.metadata().map_err(file_read_error)?.is_file() {
                return Err(CheckoutAccessError::new(
                    CheckoutAccessErrorCode::UnsafeRelativePath,
                    "relative checkout marker is not a regular file",
                ));
            }
            return Ok(true);
        }
        directory = opened;
    }
    Err(CheckoutAccessError::new(
        CheckoutAccessErrorCode::UnsafeRelativePath,
        "relative checkout marker is empty",
    ))
}

#[cfg(not(unix))]
fn regular_file_exists_beneath(
    _root: &File,
    _relative: &Path,
) -> std::result::Result<bool, CheckoutAccessError> {
    Err(CheckoutAccessError::new(
        CheckoutAccessErrorCode::ConservativePathGateDenied,
        "descriptor-relative checkout marker reads are unavailable on this platform",
    ))
}

#[cfg(unix)]
fn read_json_directory_beneath(
    root: &File,
    relative: &Path,
) -> std::result::Result<BTreeMap<String, Vec<u8>>, CheckoutAccessError> {
    use std::ffi::{CStr, CString};
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
    use std::os::unix::ffi::OsStrExt;

    let mut directory = root.try_clone().map_err(file_read_error)?;
    for component in relative.components() {
        let Component::Normal(name) = component else {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout directory contains an unsafe component",
            ));
        };
        let name = CString::new(name.as_bytes()).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "relative checkout directory contains a NUL byte",
            )
        })?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(BTreeMap::new());
            }
            return Err(file_read_error(error));
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }

    let raw_directory = directory.into_raw_fd();
    let stream = unsafe { libc::fdopendir(raw_directory) };
    if stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe {
            libc::close(raw_directory);
        }
        return Err(file_read_error(error));
    }
    struct DirectoryStream(*mut libc::DIR);
    impl Drop for DirectoryStream {
        fn drop(&mut self) {
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = DirectoryStream(stream);
    let directory_fd = unsafe { libc::dirfd(stream.0) };
    if directory_fd < 0 {
        return Err(file_read_error(std::io::Error::last_os_error()));
    }

    let mut files = BTreeMap::new();
    loop {
        unsafe {
            *checkout_access_errno_location() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error().is_some_and(|code| code != 0) {
                return Err(file_read_error(error));
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        let name_bytes = name.to_bytes();
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        let filename = std::str::from_utf8(name_bytes).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "checkout JSON directory contains a non-UTF-8 entry name",
            )
        })?;
        if Path::new(filename)
            .extension()
            .and_then(|value| value.to_str())
            != Some("json")
        {
            continue;
        }
        let filename_c = CString::new(name_bytes).map_err(|_| {
            CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "checkout JSON entry name contains a NUL byte",
            )
        })?;
        let fd = unsafe {
            libc::openat(
                directory_fd,
                filename_c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(file_read_error(std::io::Error::last_os_error()));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata().map_err(file_read_error)?.is_file() {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::UnsafeRelativePath,
                "checkout JSON entry is not a regular file",
            ));
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).map_err(file_read_error)?;
        files.insert(filename.to_owned(), bytes);
    }
    Ok(files)
}

#[cfg(all(unix, target_vendor = "apple"))]
unsafe fn checkout_access_errno_location() -> *mut libc::c_int {
    unsafe { libc::__error() }
}

#[cfg(all(unix, not(target_vendor = "apple")))]
unsafe fn checkout_access_errno_location() -> *mut libc::c_int {
    unsafe { libc::__errno_location() }
}

#[cfg(not(unix))]
fn read_json_directory_beneath(
    _root: &File,
    _relative: &Path,
) -> std::result::Result<BTreeMap<String, Vec<u8>>, CheckoutAccessError> {
    Err(CheckoutAccessError::new(
        CheckoutAccessErrorCode::ConservativePathGateDenied,
        "descriptor-relative checkout directory reads are unavailable on this platform",
    ))
}

fn file_read_error(error: std::io::Error) -> CheckoutAccessError {
    CheckoutAccessError::new(
        CheckoutAccessErrorCode::ConservativePathGateDenied,
        format!("descriptor-relative checkout read failed: {error}"),
    )
}

fn validate_id(field: &str, value: &str) -> std::result::Result<(), CheckoutAccessError> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || value.chars().any(char::is_whitespace)
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::InvalidRequest,
            format!("{field} must be a bounded non-path identifier"),
        ));
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> std::result::Result<PathBuf, CheckoutAccessError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        CheckoutAccessError::new(
            CheckoutAccessErrorCode::InvalidRoot,
            "attachment root is missing or cannot be canonicalized",
        )
    })?;
    if !canonical.is_dir() {
        return Err(CheckoutAccessError::new(
            CheckoutAccessErrorCode::InvalidRoot,
            "attachment root is not a directory",
        ));
    }
    Ok(canonical)
}

fn observation_error(error: anyhow::Error) -> CheckoutAccessError {
    CheckoutAccessError::new(
        CheckoutAccessErrorCode::ObservationUnavailable,
        format!("checkout access observation could not be committed: {error:#}"),
    )
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum CheckoutAccessOutcome {
    Granted,
    Denied,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CounterKey {
    kind: CheckoutAccessKind,
    source_lane: CheckoutAccessSourceLane,
    outcome: CheckoutAccessOutcome,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckoutAccessCounter {
    pub kind: CheckoutAccessKind,
    pub source_lane: CheckoutAccessSourceLane,
    pub outcome: CheckoutAccessOutcome,
    pub count: u64,
    pub last_sequence: u64,
    pub last_unix_secs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutAccessObservationSnapshot {
    version: u32,
    sequence: u64,
    counters: Vec<CheckoutAccessCounter>,
}

impl Default for CheckoutAccessObservationSnapshot {
    fn default() -> Self {
        Self {
            version: OBSERVATION_VERSION,
            sequence: 0,
            counters: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckoutAccessOperationHealth {
    pub kind: CheckoutAccessKind,
    pub granted: u64,
    pub denied: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_success_unix_secs: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckoutAccessHealth {
    pub sequence: u64,
    pub operations: Vec<CheckoutAccessOperationHealth>,
    pub counters: Vec<CheckoutAccessCounter>,
    pub active_compatibility_lanes: Vec<CheckoutAccessSourceLane>,
}

/// Cloneable observation handle shared by all broker instances and doctor.
/// The persisted key-space is bounded by closed enums and never accepts a path,
/// project id, attachment id, or other high-cardinality label.
#[derive(Clone)]
pub struct CheckoutAccessObservations {
    store_path: Option<Arc<PathBuf>>,
    state: Arc<Mutex<CheckoutAccessObservationSnapshot>>,
}

impl CheckoutAccessObservations {
    pub fn open(store_path: impl Into<PathBuf>) -> Result<Self> {
        let store_path = store_path.into();
        let snapshot = if store_path.exists() {
            let raw = std::fs::read_to_string(&store_path)
                .with_context(|| format!("reading {}", store_path.display()))?;
            let snapshot: CheckoutAccessObservationSnapshot = serde_json::from_str(&raw)
                .with_context(|| format!("parsing {}", store_path.display()))?;
            validate_snapshot(&snapshot)
                .with_context(|| format!("validating {}", store_path.display()))?;
            snapshot
        } else {
            CheckoutAccessObservationSnapshot::default()
        };
        Ok(Self {
            store_path: Some(Arc::new(store_path)),
            state: Arc::new(Mutex::new(snapshot)),
        })
    }

    pub fn in_memory() -> Self {
        Self {
            store_path: None,
            state: Arc::new(Mutex::new(CheckoutAccessObservationSnapshot::default())),
        }
    }

    pub fn health(&self) -> CheckoutAccessHealth {
        health_from_snapshot(&self.state.lock())
    }

    fn record(
        &self,
        kind: CheckoutAccessKind,
        source_lane: CheckoutAccessSourceLane,
        outcome: CheckoutAccessOutcome,
    ) -> Result<u64> {
        let mut state = self.state.lock();
        let mut next = state.clone();
        next.sequence = next
            .sequence
            .checked_add(1)
            .context("checkout access observation sequence exhausted")?;
        let sequence = next.sequence;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(counter) = next.counters.iter_mut().find(|counter| {
            counter.kind == kind && counter.source_lane == source_lane && counter.outcome == outcome
        }) {
            counter.count = counter
                .count
                .checked_add(1)
                .context("checkout access counter exhausted")?;
            counter.last_sequence = sequence;
            counter.last_unix_secs = now;
        } else {
            next.counters.push(CheckoutAccessCounter {
                kind,
                source_lane,
                outcome,
                count: 1,
                last_sequence: sequence,
                last_unix_secs: now,
            });
            next.counters.sort_by_key(|counter| CounterKey {
                kind: counter.kind,
                source_lane: counter.source_lane,
                outcome: counter.outcome,
            });
        }
        validate_snapshot(&next)?;
        if let Some(store_path) = &self.store_path {
            with_store_lock(store_path, || {
                atomic_write_json_locked(store_path, &next)?;
                sync_parent_directory(store_path)
            })?;
        }
        *state = next;
        Ok(sequence)
    }
}

fn validate_snapshot(snapshot: &CheckoutAccessObservationSnapshot) -> Result<()> {
    if snapshot.version != OBSERVATION_VERSION {
        anyhow::bail!(
            "unsupported checkout access observation version {}",
            snapshot.version
        );
    }
    let maximum_counters = CheckoutAccessKind::ALL.len() * CheckoutAccessSourceLane::ALL.len() * 2;
    if snapshot.counters.len() > maximum_counters {
        anyhow::bail!("checkout access observation counter set is not bounded");
    }
    let mut keys = BTreeSet::new();
    for counter in &snapshot.counters {
        if counter.count == 0
            || counter.last_sequence == 0
            || counter.last_sequence > snapshot.sequence
        {
            anyhow::bail!("invalid checkout access observation counter");
        }
        let key = CounterKey {
            kind: counter.kind,
            source_lane: counter.source_lane,
            outcome: counter.outcome,
        };
        if !keys.insert(key) {
            anyhow::bail!("duplicate checkout access observation counter");
        }
    }
    Ok(())
}

fn health_from_snapshot(snapshot: &CheckoutAccessObservationSnapshot) -> CheckoutAccessHealth {
    let operations = CheckoutAccessKind::ALL
        .into_iter()
        .map(|kind| {
            let mut granted = 0_u64;
            let mut denied = 0_u64;
            let mut last_success_unix_secs: Option<u64> = None;
            for counter in snapshot
                .counters
                .iter()
                .filter(|counter| counter.kind == kind)
            {
                match counter.outcome {
                    CheckoutAccessOutcome::Granted => {
                        granted = granted.saturating_add(counter.count);
                        last_success_unix_secs = Some(
                            last_success_unix_secs
                                .unwrap_or_default()
                                .max(counter.last_unix_secs),
                        );
                    }
                    CheckoutAccessOutcome::Denied => {
                        denied = denied.saturating_add(counter.count);
                    }
                }
            }
            CheckoutAccessOperationHealth {
                kind,
                granted,
                denied,
                last_success_unix_secs,
            }
        })
        .collect();
    let active_compatibility_lanes = CheckoutAccessSourceLane::ALL
        .into_iter()
        .filter(|lane| {
            lane.is_compatibility()
                && snapshot.counters.iter().any(|counter| {
                    counter.source_lane == *lane
                        && counter.outcome == CheckoutAccessOutcome::Granted
                        && counter.count > 0
                })
        })
        .collect();
    CheckoutAccessHealth {
        sequence: snapshot.sequence,
        operations,
        counters: snapshot.counters.clone(),
        active_compatibility_lanes,
    }
}

fn sync_parent_directory(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)
        .with_context(|| format!("opening {} for fsync", parent.display()))?
        .sync_all()
        .with_context(|| format!("fsyncing {}", parent.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone)]
    struct TestAuthority {
        candidate: CheckoutAccessCandidate,
        allow_gate: bool,
    }

    impl CheckoutAccessAuthority for TestAuthority {
        fn resolve(
            &self,
            _request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            Ok(self.candidate.clone())
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            self.allow_gate.then_some(()).ok_or_else(|| {
                CheckoutAccessError::new(
                    CheckoutAccessErrorCode::ConservativePathGateDenied,
                    "test path gate denied access",
                )
            })
        }
    }

    #[derive(Clone)]
    struct MutableAuthority {
        candidate: Arc<Mutex<CheckoutAccessCandidate>>,
    }

    impl CheckoutAccessAuthority for MutableAuthority {
        fn resolve(
            &self,
            _request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            Ok(self.candidate.lock().clone())
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            Ok(())
        }
    }

    fn scope() -> PublishedScope {
        PublishedScope {
            repo_id: "repo-1".into(),
            bbox_root_relpath: ".".into(),
        }
    }

    fn request(kind: CheckoutAccessKind, intent: CheckoutAccessIntent) -> CheckoutAccessRequest {
        CheckoutAccessRequest {
            project_id: "project-1".into(),
            attachment: CheckoutAttachmentSelector::AttachmentId("attachment-1".into()),
            expected_scope: Some(scope()),
            kind,
            intent,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        }
    }

    fn authority(root: &Path, kind: CheckoutAccessKind) -> TestAuthority {
        TestAuthority {
            candidate: CheckoutAccessCandidate {
                project_id: "project-1".into(),
                attachment_id: "attachment-1".into(),
                checkout_id: "checkout-1".into(),
                published_scope: Some(scope()),
                branch_ref: Some("refs/heads/main".into()),
                checkout_root: root.to_path_buf(),
                project_root: root.join("project"),
                status: CheckoutAttachmentStatus::Active,
                capabilities: BTreeSet::from([kind]),
                lifetime_guard: None,
            },
            allow_gate: true,
        }
    }

    #[test]
    fn broker_returns_lease_only_after_full_validation_and_observation() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        std::fs::write(root.join("project/file.txt"), "ok").unwrap();
        let observations = CheckoutAccessObservations::in_memory();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::LocalProjectWalk)),
            observations,
        );

        let lease = broker
            .acquire(request(
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();

        assert_eq!(lease.project_id(), "project-1");
        assert_eq!(lease.acquisition_sequence(), 1);
        assert_eq!(
            lease.resolve_existing("file.txt").unwrap(),
            root.join("project/file.txt")
        );
        let operation = broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::LocalProjectWalk)
            .unwrap();
        assert_eq!(operation.granted, 1);
        assert_eq!(operation.denied, 0);
        assert!(operation.last_success_unix_secs.is_some());
    }

    #[test]
    fn lease_captures_json_directory_through_retained_descriptor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let knowledge = project.join(".bbox/knowledge");
        std::fs::create_dir_all(&knowledge).unwrap();
        std::fs::write(knowledge.join("one.json"), br#"{"id":"one"}"#).unwrap();
        std::fs::write(knowledge.join("ignored.txt"), b"ignored").unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(
                &root,
                CheckoutAccessKind::KnowledgeGapOverlayRead,
            )),
            CheckoutAccessObservations::in_memory(),
        );
        let lease = broker
            .acquire(request(
                CheckoutAccessKind::KnowledgeGapOverlayRead,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();
        let files = lease
            .read_relative_json_directory(".bbox/knowledge")
            .unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files.get("one.json").unwrap(), br#"{"id":"one"}"#);
        assert!(
            lease
                .read_relative_json_directory(".bbox/gaps")
                .unwrap()
                .is_empty()
        );
    }

    #[cfg(unix)]
    #[test]
    fn lease_json_directory_rejects_symlinked_json_entry() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("project");
        let knowledge = project.join(".bbox/knowledge");
        std::fs::create_dir_all(&knowledge).unwrap();
        let outside = root.join("outside.json");
        std::fs::write(&outside, b"{}").unwrap();
        symlink(&outside, knowledge.join("escape.json")).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(
                &root,
                CheckoutAccessKind::KnowledgeGapOverlayRead,
            )),
            CheckoutAccessObservations::in_memory(),
        );
        let lease = broker
            .acquire(request(
                CheckoutAccessKind::KnowledgeGapOverlayRead,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            lease
                .read_relative_json_directory(".bbox/knowledge")
                .unwrap_err()
                .code,
            CheckoutAccessErrorCode::ConservativePathGateDenied
        );
    }

    #[test]
    fn mutation_pin_and_lifecycle_change_exclude_each_other_without_waiting() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::RepositoryMutation)),
            CheckoutAccessObservations::in_memory(),
        );
        let mutation = broker
            .acquire(request(
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap();
        assert_eq!(
            broker.lifecycle_mutation_guard().unwrap_err().code,
            CheckoutAccessErrorCode::LifecycleBusy
        );
        drop(mutation);

        let lifecycle = broker.lifecycle_mutation_guard().unwrap();
        assert_eq!(
            broker
                .acquire(request(
                    CheckoutAccessKind::RepositoryMutation,
                    CheckoutAccessIntent::Write,
                ))
                .unwrap_err()
                .code,
            CheckoutAccessErrorCode::LifecycleBusy
        );
        drop(lifecycle);

        assert!(
            broker
                .acquire(request(
                    CheckoutAccessKind::RepositoryMutation,
                    CheckoutAccessIntent::Write,
                ))
                .is_ok()
        );
    }

    #[test]
    fn publication_guard_closes_revalidate_to_publish_lifecycle_window() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::LocalProjectWalk)),
            CheckoutAccessObservations::in_memory(),
        );
        let lease = broker
            .acquire(request(
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();

        let publication = broker.publication_guard(&lease).unwrap();
        assert_eq!(
            broker.lifecycle_mutation_guard().unwrap_err().code,
            CheckoutAccessErrorCode::LifecycleBusy
        );
        drop(publication);

        let lifecycle = broker.lifecycle_mutation_guard().unwrap();
        assert_eq!(
            broker.publication_guard(&lease).unwrap_err().code,
            CheckoutAccessErrorCode::LifecycleBusy
        );
        drop(lifecycle);
        assert!(broker.publication_guard(&lease).is_ok());
    }

    #[test]
    fn lease_revalidation_observes_detach_before_publication() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let candidate = Arc::new(Mutex::new(
            authority(&root, CheckoutAccessKind::LocalProjectWalk).candidate,
        ));
        let broker = CheckoutAccessBroker::new(
            Arc::new(MutableAuthority {
                candidate: candidate.clone(),
            }),
            CheckoutAccessObservations::in_memory(),
        );
        let lease = broker
            .acquire(request(
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();
        candidate.lock().status = CheckoutAttachmentStatus::Detached;

        assert_eq!(
            broker.revalidate(&lease).unwrap_err().code,
            CheckoutAccessErrorCode::AttachmentInactive
        );
    }

    #[test]
    fn access_kind_intent_matrix_keeps_render_as_its_own_write_capability() {
        for kind in CheckoutAccessKind::ALL {
            let permits_read = kind.permits(CheckoutAccessIntent::Read);
            let permits_write = kind.permits(CheckoutAccessIntent::Write);
            match kind {
                CheckoutAccessKind::RenderFileProvider | CheckoutAccessKind::ProvenanceNoteIo => {
                    assert!(permits_read, "{} must permit reads", kind.as_str());
                    assert!(permits_write, "{} must permit writes", kind.as_str());
                }
                CheckoutAccessKind::RepositoryMutation => {
                    assert!(!permits_read, "repository mutation is write-only");
                    assert!(permits_write, "repository mutation must permit writes");
                }
                _ => {
                    assert!(permits_read, "{} must permit reads", kind.as_str());
                    assert!(!permits_write, "{} must reject writes", kind.as_str());
                }
            }
        }
    }

    #[test]
    fn deny_probe_returns_no_lease_and_records_zero_acquisitions() {
        let observations = CheckoutAccessObservations::in_memory();
        let broker = CheckoutAccessBroker::new(Arc::new(DenyCheckoutAccess), observations);

        let error = broker
            .acquire(request(
                CheckoutAccessKind::GitHistory,
                CheckoutAccessIntent::Read,
            ))
            .unwrap_err();

        assert_eq!(error.code, CheckoutAccessErrorCode::DeniedByTestProbe);
        let operation = broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::GitHistory)
            .unwrap();
        assert_eq!(operation.granted, 0);
        assert_eq!(operation.denied, 1);
        assert_eq!(operation.last_success_unix_secs, None);
    }

    #[test]
    fn scope_capability_status_and_path_gate_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();

        let mut inactive = authority(&root, CheckoutAccessKind::GitHistory);
        inactive.candidate.status = CheckoutAttachmentStatus::Detached;
        let error =
            CheckoutAccessBroker::new(Arc::new(inactive), CheckoutAccessObservations::in_memory())
                .acquire(request(
                    CheckoutAccessKind::GitHistory,
                    CheckoutAccessIntent::Read,
                ))
                .unwrap_err();
        assert_eq!(error.code, CheckoutAccessErrorCode::AttachmentInactive);

        let mut wrong_scope = authority(&root, CheckoutAccessKind::GitHistory);
        wrong_scope.candidate.published_scope = None;
        let error = CheckoutAccessBroker::new(
            Arc::new(wrong_scope),
            CheckoutAccessObservations::in_memory(),
        )
        .acquire(request(
            CheckoutAccessKind::GitHistory,
            CheckoutAccessIntent::Read,
        ))
        .unwrap_err();
        assert_eq!(error.code, CheckoutAccessErrorCode::ScopeMismatch);

        let missing_capability = authority(&root, CheckoutAccessKind::Blame);
        let error = CheckoutAccessBroker::new(
            Arc::new(missing_capability),
            CheckoutAccessObservations::in_memory(),
        )
        .acquire(request(
            CheckoutAccessKind::GitHistory,
            CheckoutAccessIntent::Read,
        ))
        .unwrap_err();
        assert_eq!(error.code, CheckoutAccessErrorCode::CapabilityDenied);

        let mut denied_gate = authority(&root, CheckoutAccessKind::GitHistory);
        denied_gate.allow_gate = false;
        let error = CheckoutAccessBroker::new(
            Arc::new(denied_gate),
            CheckoutAccessObservations::in_memory(),
        )
        .acquire(request(
            CheckoutAccessKind::GitHistory,
            CheckoutAccessIntent::Read,
        ))
        .unwrap_err();
        assert_eq!(
            error.code,
            CheckoutAccessErrorCode::ConservativePathGateDenied
        );
    }

    #[test]
    fn scope_discovery_is_restricted_to_the_publisher_selected_legacy_lane() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let make_broker = || {
            CheckoutAccessBroker::new(
                Arc::new(authority(
                    &root,
                    CheckoutAccessKind::PublisherConfigTreeRead,
                )),
                CheckoutAccessObservations::in_memory(),
            )
        };
        let discovery = CheckoutAccessRequest {
            project_id: "project-1".into(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        };
        assert_eq!(
            make_broker()
                .acquire(discovery.clone())
                .unwrap()
                .published_scope(),
            Some(&scope())
        );

        let mut wrong_kind = discovery.clone();
        wrong_kind.kind = CheckoutAccessKind::LocalProjectWalk;
        assert_eq!(
            make_broker().acquire(wrong_kind).unwrap_err().code,
            CheckoutAccessErrorCode::ScopeMismatch
        );
        let mut wrong_selector = discovery.clone();
        wrong_selector.attachment = CheckoutAttachmentSelector::AttachmentId("attachment-1".into());
        assert_eq!(
            make_broker().acquire(wrong_selector).unwrap_err().code,
            CheckoutAccessErrorCode::ScopeMismatch
        );
        let mut wrong_lane = discovery;
        wrong_lane.source_lane = CheckoutAccessSourceLane::NativeAttachment;
        assert_eq!(
            make_broker().acquire(wrong_lane).unwrap_err().code,
            CheckoutAccessErrorCode::ScopeMismatch
        );
    }

    #[test]
    fn lease_rejects_parent_traversal_and_read_lease_write_targets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::LocalProjectWalk)),
            CheckoutAccessObservations::in_memory(),
        );
        let lease = broker
            .acquire(request(
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();

        assert_eq!(
            lease.resolve_existing("../outside").unwrap_err().code,
            CheckoutAccessErrorCode::UnsafeRelativePath
        );
        assert_eq!(
            lease.resolve_write_target("new.txt").unwrap_err().code,
            CheckoutAccessErrorCode::WriteIntentRequired
        );
    }

    #[cfg(unix)]
    #[test]
    fn lease_rejects_symlink_escape_for_reads_and_writes() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("project")).unwrap();
        std::fs::create_dir(root.join("outside")).unwrap();
        std::fs::write(root.join("outside/file.txt"), "outside").unwrap();
        symlink(root.join("outside"), root.join("project/escape")).unwrap();

        let read_broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::LocalProjectWalk)),
            CheckoutAccessObservations::in_memory(),
        );
        let read_lease = read_broker
            .acquire(request(
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();
        assert_eq!(
            read_lease
                .resolve_existing("escape/file.txt")
                .unwrap_err()
                .code,
            CheckoutAccessErrorCode::UnsafeRelativePath
        );

        let write_broker = CheckoutAccessBroker::new(
            Arc::new(authority(&root, CheckoutAccessKind::RepositoryMutation)),
            CheckoutAccessObservations::in_memory(),
        );
        let write_lease = write_broker
            .acquire(request(
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap();
        assert_eq!(
            write_lease
                .resolve_write_target("escape/new.txt")
                .unwrap_err()
                .code,
            CheckoutAccessErrorCode::UnsafeRelativePath
        );
    }

    #[test]
    fn observation_snapshot_is_bounded_path_free_and_rolls_forward() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("checkout-access-observations.json");
        let observations = CheckoutAccessObservations::open(&path).unwrap();
        observations
            .record(
                CheckoutAccessKind::Blame,
                CheckoutAccessSourceLane::LegacyProjectRecord,
                CheckoutAccessOutcome::Granted,
            )
            .unwrap();
        observations
            .record(
                CheckoutAccessKind::Blame,
                CheckoutAccessSourceLane::LegacyProjectRecord,
                CheckoutAccessOutcome::Denied,
            )
            .unwrap();
        drop(observations);

        let reopened = CheckoutAccessObservations::open(&path).unwrap();
        assert_eq!(reopened.health().sequence, 2);
        reopened
            .record(
                CheckoutAccessKind::GitHistory,
                CheckoutAccessSourceLane::NativeAttachment,
                CheckoutAccessOutcome::Granted,
            )
            .unwrap();
        let health = reopened.health();
        assert_eq!(health.sequence, 3);
        assert_eq!(
            health.active_compatibility_lanes,
            vec![CheckoutAccessSourceLane::LegacyProjectRecord]
        );
        assert!(
            health.counters.len()
                <= CheckoutAccessKind::ALL.len() * CheckoutAccessSourceLane::ALL.len() * 2
        );

        let persisted = std::fs::read_to_string(path).unwrap();
        assert!(!persisted.contains(root.to_string_lossy().as_ref()));
        assert!(!persisted.contains("project-1"));
        assert!(!persisted.contains("attachment-1"));
    }
}

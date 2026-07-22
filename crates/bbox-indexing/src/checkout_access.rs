//! Validated checkout access and bounded operational observations.
//!
//! Checkout paths are host authority, not corpus identity. This module keeps
//! that authority behind a broker: callers name a logical project and an
//! attachment selector, an injected authority adapter resolves the current
//! attachment state, and the broker independently checks identity, scope,
//! capability, intent, path containment, and the conservative path gate before
//! returning a lease. The broker deliberately has no dependency on the legacy
//! `ProjectRecord` shape.

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{atomic_write_json_locked, with_store_lock};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const OBSERVATION_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 256;

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
            Self::ProvenanceNoteIo => true,
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
}

impl CheckoutAccessSourceLane {
    pub const ALL: [Self; 3] = [
        Self::NativeAttachment,
        Self::LegacyProjectRecord,
        Self::LegacyCheckoutRegistry,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NativeAttachment => "native_attachment",
            Self::LegacyProjectRecord => "legacy_project_record",
            Self::LegacyCheckoutRegistry => "legacy_checkout_registry",
        }
    }

    pub const fn is_compatibility(self) -> bool {
        matches!(
            self,
            Self::LegacyProjectRecord | Self::LegacyCheckoutRegistry
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
}

/// Complete request presented to the broker. `expected_scope = None` is valid
/// only for a legacy local project with no durable published scope.
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
    pub checkout_root: PathBuf,
    pub project_root: PathBuf,
    pub status: CheckoutAttachmentStatus,
    pub capabilities: BTreeSet<CheckoutAccessKind>,
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
        checkout_root: &Path,
        project_root: &Path,
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
        _checkout_root: &Path,
        _project_root: &Path,
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
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
    source_lane: CheckoutAccessSourceLane,
    checkout_root: PathBuf,
    project_root: PathBuf,
    acquisition_sequence: u64,
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
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
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
}

impl CheckoutAccessBroker {
    pub fn new(
        authority: Arc<dyn CheckoutAccessAuthority>,
        observations: CheckoutAccessObservations,
    ) -> Self {
        Self {
            authority,
            observations,
        }
    }

    pub fn acquire(
        &self,
        request: CheckoutAccessRequest,
    ) -> std::result::Result<ValidatedCheckoutLease, CheckoutAccessError> {
        let result = self.acquire_unobserved(&request);
        match result {
            Ok(candidate) => {
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
                    kind: request.kind,
                    intent: request.intent,
                    source_lane: request.source_lane,
                    checkout_root: candidate.checkout_root,
                    project_root: candidate.project_root,
                    acquisition_sequence: sequence,
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

    fn acquire_unobserved(
        &self,
        request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        validate_id("project_id", &request.project_id)?;
        match &request.attachment {
            CheckoutAttachmentSelector::Selected => {}
            CheckoutAttachmentSelector::AttachmentId(value) => {
                validate_id("attachment_id", value)?;
            }
            CheckoutAttachmentSelector::CheckoutId(value) => {
                validate_id("checkout_id", value)?;
            }
        }
        if !request.kind.permits(request.intent) {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::IntentDenied,
                "the requested access kind does not permit this intent",
            ));
        }

        let mut candidate = self.authority.resolve(request)?;
        validate_id("candidate project_id", &candidate.project_id)?;
        validate_id("candidate attachment_id", &candidate.attachment_id)?;
        validate_id("candidate checkout_id", &candidate.checkout_id)?;
        if candidate.project_id != request.project_id {
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
            _ => {}
        }
        if candidate.status != CheckoutAttachmentStatus::Active {
            return Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::AttachmentInactive,
                "the resolved attachment is not active",
            ));
        }
        if candidate.published_scope != request.expected_scope {
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
        self.authority
            .revalidate_conservative_path_gate(request, &checkout_root, &project_root)?;
        candidate.checkout_root = checkout_root;
        candidate.project_root = project_root;
        Ok(candidate)
    }
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
            _checkout_root: &Path,
            _project_root: &Path,
        ) -> std::result::Result<(), CheckoutAccessError> {
            self.allow_gate.then_some(()).ok_or_else(|| {
                CheckoutAccessError::new(
                    CheckoutAccessErrorCode::ConservativePathGateDenied,
                    "test path gate denied access",
                )
            })
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
                checkout_root: root.to_path_buf(),
                project_root: root.join("project"),
                status: CheckoutAttachmentStatus::Active,
                capabilities: BTreeSet::from([kind]),
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

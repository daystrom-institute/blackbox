//! Strict paired persistence for the durable project catalog and host-local
//! attachment snapshot.
//!
//! The catalog and attachment files are one logical value. Every mutation is
//! journaled, installs both post-images, and publishes in-memory state only
//! after the installed pair has been read back and cross-validated.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
#[cfg(any(test, not(unix)))]
use std::fs;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[cfg(test)]
use bbox_code_source_store::encode_migration_effective_source_manifest_v1;
use bbox_code_source_store::{
    CodeSourceStorePaths, CollisionRetirementLifecycleStateV1, CollisionRetirementLifecycleV1,
    CollisionRetirementSelectorEvidenceV1, MAX_MIGRATION_INVENTORY_GENERATIONS,
    MigrationEffectiveSourceManifestV1, MigrationLegacyAnchorEvidenceV1,
    MigrationLegacyInventoryV1, StoreLimits, decode_activation_v1_for_migration,
    decode_activation_v2_for_migration, decode_collision_retirement_pending_for_migration,
    decode_migration_effective_source_manifest_v1, decode_stored_generation_v1_for_migration,
    decode_stored_generation_v2_for_migration,
    enumerate_current_migration_inventory_for_scopes_locked,
    enumerate_legacy_migration_inventory_for_scopes_locked,
    verify_generation_manifest_for_migration,
};
use bbox_corpus_core::identity::CHECKOUT_LOCAL_GITIGNORE_BYTES;
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::json_store::{
    NofollowDirectory, StoreLockGuard, acquire_store_lock_nofollow, canonical_store_lock_path,
};
use bbox_corpus_core::project_catalog::{
    AttachmentId, AttachmentSnapshotV1, AttachmentStatus, CatalogOriginV2, CatalogSnapshotV2,
    CheckoutAttachment, MAX_LEGACY_PROJECT_STORE_BYTES, MAX_PROJECT_CATALOG_BYTES,
    MAX_PROJECT_CATALOG_ENTRIES, ProjectCatalogTransactionId, ProjectId, ProjectScope,
    decode_attachment_snapshot, decode_catalog_snapshot, decode_legacy_project_store,
    encode_attachment_snapshot, encode_catalog_snapshot, validate_catalog_attachments,
};
use parking_lot::RwLock;
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};

use crate::accepted_publication_store::{
    AcceptedPublicationGenerationId, AcceptedPublicationLimits, AcceptedPublicationStorePaths,
    FullPublisherRef, GitObjectId, MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES,
    MAX_ACCEPTED_PUBLICATION_POINTER_BYTES, decode_generation_v1, decode_pointer_v1,
    verify_pointer_generation_v1,
};
use crate::project_catalog_inventory::ValidatedQuarantineBindingsV1;
use crate::project_catalog_migration_lock::{
    ProjectCatalogMigrationLock, project_catalog_migration_lock_path,
};
use crate::publisher::{MAX_PUBLISHER_REF_ROWS, PublisherRefRow, decode_publisher_ref_source_v1};

const JOURNAL_VERSION: u32 = 1;
const MIGRATION_MARKER_VERSION: u32 = 1;
const MAX_MIGRATION_PARTICIPANTS: usize =
    MAX_MIGRATION_INVENTORY_GENERATIONS + MAX_PROJECT_CATALOG_ENTRIES * 3 + 4;
// The fixed margin covers the singleton immutable assets that exist at most
// once per migration regardless of generation/publisher counts:
// LegacyProjectStoreBackup, LegacyPublisherRefBackup, and (Phase 3)
// LegacyCommitNamespaceInventory.
const MAX_MIGRATION_IMMUTABLE_ASSETS: usize =
    MAX_MIGRATION_INVENTORY_GENERATIONS + MAX_PUBLISHER_REF_ROWS + 3;
const MAX_MIGRATION_CHECKOUT_ACTIONS: usize = MAX_PROJECT_CATALOG_ENTRIES;
const MAX_MIGRATION_PUBLISHER_PINS: usize = MAX_PUBLISHER_REF_ROWS;
// Publisher rows have their own aggregate serialized budget. All other
// variable durable evidence is charged once under the structural budget. The
// fixed margin covers scalar envelope fields, collection delimiters, and the
// final newline in either pretty-JSON artifact.
const MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES: usize = 128 * 1024 * 1024;
const MAX_JOURNAL_BYTES: usize = 512 * 1024 * 1024;
const MAX_MARKER_BYTES: usize = 512 * 1024 * 1024;
const MAX_MIGRATION_DURABLE_ENVELOPE_BYTES: usize = 1024 * 1024;
const MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES: usize = MAX_JOURNAL_BYTES
    - MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES
    - MAX_MIGRATION_DURABLE_ENVELOPE_BYTES;
const MAX_CODE_SOURCE_EFFECTIVE_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MAX_CODE_SOURCE_ACTIVATION_BYTES: usize = 512 * 1024 * 1024;
const MAX_CODE_SOURCE_GENERATION_METADATA_BYTES: usize = 64 * 1024;
const MAX_CODE_SOURCE_COLLISION_RETIREMENT_BYTES: usize = 64 * 1024;
const MAX_CODE_SOURCE_COLLECTED_MANIFEST_BYTES: usize = 512 * 1024 * 1024;
const MAX_LEGACY_PUBLISHER_REF_SOURCE_BYTES: usize = MAX_PROJECT_CATALOG_BYTES;
const MAX_LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_BYTES: usize = MAX_PROJECT_CATALOG_BYTES;

pub type ProjectCatalogStoreResult<T> = Result<T, ProjectCatalogStoreError>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationMutationDispositionV1 {
    NoDurableMutation,
    RecoveredToOldState,
    RecoveredToCommittedState,
    RetryExactPlanRequired,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationTransactionFailureV1 {
    pub(crate) error: ProjectCatalogStoreError,
    pub(crate) disposition: MigrationMutationDispositionV1,
}

#[derive(Debug)]
pub(crate) struct MigrationStoreOpenV1 {
    pub(crate) store: ProjectCatalogStore,
    pub(crate) disposition: MigrationMutationDispositionV1,
}

#[derive(Debug)]
pub(crate) enum MigrationStoreOpenOutcomeV1 {
    Installed(MigrationStoreOpenV1),
    RolledBackNotInstalled {
        disposition: MigrationMutationDispositionV1,
    },
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationStoreOpenFailureV1 {
    pub(crate) error: ProjectCatalogStoreError,
    pub(crate) disposition: MigrationMutationDispositionV1,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationBootstrapFailureV1 {
    pub(crate) error: ProjectCatalogStoreError,
    pub(crate) disposition: MigrationMutationDispositionV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogStoreError {
    code: &'static str,
    detail: String,
}

impl ProjectCatalogStoreError {
    pub(crate) fn new(code: &'static str, detail: impl Into<String>) -> Self {
        let detail = detail
            .into()
            .chars()
            .map(|ch| if ch.is_control() { ' ' } else { ch })
            .take(512)
            .collect();
        Self { code, detail }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for ProjectCatalogStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for ProjectCatalogStoreError {}

fn io_error(operation: &str, path: &Path, error: impl fmt::Display) -> ProjectCatalogStoreError {
    ProjectCatalogStoreError::new(
        "error.project_catalog_io",
        format!("{operation} {} failed: {error}", path.display()),
    )
}

fn contract_error(error: impl fmt::Display) -> ProjectCatalogStoreError {
    ProjectCatalogStoreError::new("error.project_catalog_invalid_snapshot", error.to_string())
}

#[derive(Debug, Clone)]
pub struct ProjectCatalogState {
    catalog: Arc<CatalogSnapshotV2>,
    attachments: Arc<AttachmentSnapshotV1>,
    epoch: u64,
    catalog_sha256: Sha256Hex,
    attachments_sha256: Sha256Hex,
}

impl ProjectCatalogState {
    pub fn catalog(&self) -> &Arc<CatalogSnapshotV2> {
        &self.catalog
    }

    pub fn attachments(&self) -> &Arc<AttachmentSnapshotV1> {
        &self.attachments
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    pub fn catalog_sha256(&self) -> &str {
        self.catalog_sha256.as_str()
    }

    pub fn attachments_sha256(&self) -> &str {
        self.attachments_sha256.as_str()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogCommit {
    pub epoch: u64,
    pub catalog_sha256: String,
    pub attachments_sha256: String,
}

/// Emitted by the post-commit observer after a successful `transact`
/// (section 9.4). The server maps each affected project id to one
/// reconciler event.
#[derive(Debug, Clone)]
pub struct CatalogCommittedEvent {
    pub epoch: u64,
    pub changed_project_ids: BTreeSet<String>,
}

/// Cloneable observer handle for post-commit notifications (section 9.4).
///
/// On successful `transact`, after durable pair publication and lock
/// release, the store pushes a `CatalogCommittedEvent` into the shared
/// queue. Callers poll `drain_events` to consume them. The observer
/// does not carry mutable records. Delivery failure marks health and
/// triggers one bounded rescan (R5).
#[derive(Clone)]
pub struct CatalogCommitObserver {
    queue: Arc<std::sync::Mutex<CatalogObserverQueue>>,
}

#[derive(Default)]
struct CatalogObserverQueue {
    event: Option<CatalogCommittedEvent>,
    rescan_required: bool,
    rescan_generation: u64,
    rescan_followup_required: bool,
}

impl CatalogCommitObserver {
    pub fn new() -> Self {
        Self {
            queue: Arc::new(std::sync::Mutex::new(CatalogObserverQueue::default())),
        }
    }

    /// Push a commit event into the observer queue (section 9.4).
    /// Normally called by `transact`; exposed for testing.
    #[doc(hidden)]
    pub fn push_for_test(&self, event: CatalogCommittedEvent) {
        self.push(event);
    }

    fn push(&self, event: CatalogCommittedEvent) {
        let mut guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.rescan_required {
            guard.rescan_followup_required = true;
            return;
        }
        const MAX_PENDING_PROJECTS: usize = 4096;
        let pending = guard.event.get_or_insert_with(|| CatalogCommittedEvent {
            epoch: event.epoch,
            changed_project_ids: BTreeSet::new(),
        });
        pending.epoch = pending.epoch.max(event.epoch);
        pending
            .changed_project_ids
            .extend(event.changed_project_ids);
        if pending.changed_project_ids.len() > MAX_PENDING_PROJECTS {
            guard.event = None;
            guard.rescan_required = true;
            guard.rescan_generation = guard.rescan_generation.wrapping_add(1);
        }
    }

    /// Drain all pending commit events (section 9.4).
    pub fn drain_events(&self) -> Vec<CatalogCommittedEvent> {
        let mut guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.event.take().into_iter().collect()
    }

    pub fn pending_rescan_generation(&self) -> Option<u64> {
        let guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.rescan_required.then_some(guard.rescan_generation)
    }

    pub fn complete_rescan(&self, generation: u64) -> bool {
        let mut guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.rescan_required && guard.rescan_generation == generation {
            if guard.rescan_followup_required {
                guard.rescan_followup_required = false;
                guard.rescan_generation = guard.rescan_generation.wrapping_add(1);
            } else {
                guard.rescan_required = false;
            }
            true
        } else {
            false
        }
    }

    pub fn request_rescan(&self) {
        let mut guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        if guard.rescan_required {
            guard.rescan_followup_required = true;
        } else {
            guard.rescan_required = true;
            guard.rescan_generation = guard.rescan_generation.wrapping_add(1);
        }
    }

    /// Returns true if at least one event is pending.
    pub fn has_events(&self) -> bool {
        let guard = match self.queue.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        guard.event.is_some()
    }
}

impl fmt::Debug for CatalogCommitObserver {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let len = match self.queue.lock() {
            Ok(guard) => usize::from(guard.event.is_some()),
            Err(_) => 0,
        };
        f.debug_struct("CatalogCommitObserver")
            .field("pending_events", &len)
            .finish()
    }
}

pub struct ProjectCatalogStore {
    owner: ProjectCatalogTransactionOwner,
    current: RwLock<PublishedStoreState>,
    _lifetime_lock: Arc<ProjectCatalogMigrationLock>,
    commit_observer: CatalogCommitObserver,
}

#[derive(Debug, Clone)]
enum PublishedStoreState {
    Ready(Arc<ProjectCatalogState>),
    Poisoned(ProjectCatalogStoreError),
}

impl fmt::Debug for ProjectCatalogStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self.current.read();
        let epoch = match &*state {
            PublishedStoreState::Ready(state) => Some(state.epoch),
            PublishedStoreState::Poisoned(_) => None,
        };
        formatter
            .debug_struct("ProjectCatalogStore")
            .field("catalog_path", &self.owner.paths.catalog)
            .field("epoch", &epoch)
            .finish_non_exhaustive()
    }
}

/// Outcome of the daemon's startup store-version probe (phase-2 §4.1).
/// The probe decides which runtime authority opens the store; it never
/// opens, repairs, or creates anything itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectStoreProbe {
    /// No store and no catalog-family sibling artifact: the version-1
    /// bridge creates its store exactly as today.
    AbsentBridge,
    /// A version-1 `LegacyProjectStoreV1` file: bridge mode.
    LegacyV1,
    /// A version-2 `CatalogSnapshotV2` file: catalog mode; the strict pair
    /// open (including origin/marker binding) happens in `open_existing`.
    CatalogV2,
}

/// Probe the configured projects path for the runtime authority mode.
///
/// Fail-closed rules:
/// - an absent catalog with ANY code-owned catalog-family sibling present
///   (attachment snapshot, transaction journal, committed migration marker,
///   migration receipt, migration assets, stage or backup artifacts; the
///   two lock files excluded) is a half-pair state and refuses, so the
///   bridge can never mint a fresh v1 store beside v2 authority state. The
///   sibling set is the store owner's own path-role table, not a
///   probe-local list;
/// - unreadable, oversize, malformed, or unknown-version bytes refuse.
pub fn probe_project_store_mode(
    projects_path: &Path,
) -> ProjectCatalogStoreResult<ProjectStoreProbe> {
    let paths = ProjectCatalogPaths::derive(projects_path)?;
    let catalog_present = paths.catalog.symlink_metadata().is_ok();
    if !catalog_present {
        let siblings: [(&Path, &str); 7] = [
            (&paths.attachments, "project-attachments.json"),
            (&paths.journal, "project-catalog-transaction.json"),
            (&paths.migration_marker, "project-catalog-migration.json"),
            (
                &paths.migration_receipt,
                "project-catalog-migration-receipt.json",
            ),
            (
                &paths.migration_assets_dir,
                "project-catalog-migration-assets",
            ),
            (&paths.stage_dir, "project-catalog-stage"),
            (&paths.backup_dir, "project-catalog-backups"),
        ];
        for (path, role) in siblings {
            if path.symlink_metadata().is_ok() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_half_pair",
                    format!(
                        "projects store is absent but catalog-family sibling {role} exists; \
                         refusing to select a mode over half-pair state"
                    ),
                ));
            }
        }
        return Ok(ProjectStoreProbe::AbsentBridge);
    }
    let Some(raw) =
        RealCatalogStoreIo.read_regular_nofollow(&paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?
    else {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_snapshot",
            "projects store disappeared between presence check and read",
        ));
    };
    #[derive(serde::Deserialize)]
    struct VersionProbe {
        version: u64,
    }
    let probe: VersionProbe = serde_json::from_slice(&raw).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_snapshot",
            format!("projects store version probe failed: {error}"),
        )
    })?;
    match probe.version {
        1 => Ok(ProjectStoreProbe::LegacyV1),
        2 => Ok(ProjectStoreProbe::CatalogV2),
        other => Err(ProjectCatalogStoreError::new(
            "error.project_catalog_unsupported_version",
            format!("projects store version {other} is not supported"),
        )),
    }
}

/// The migration immutable-asset root for a projects store path, plus the
/// stable on-disk name prefix of the legacy commit-namespace inventory asset
/// installed by transaction `transaction_id`.
///
/// Exposed crate-internally so the Phase 3 history materializer can read
/// back the asset it must prove against without duplicating this module's
/// path-role table. The prefix is exactly what `immutable_target_name`
/// produces up to the content hash, which the reader recomputes rather than
/// trusts.
pub(crate) fn legacy_commit_namespace_inventory_asset_location(
    projects_path: &Path,
    transaction_id: &ProjectCatalogTransactionId,
) -> ProjectCatalogStoreResult<(PathBuf, String)> {
    let paths = ProjectCatalogPaths::derive(projects_path)?;
    let prefix = format!(
        "{}.{}.",
        transaction_id,
        ImmutableAssetRoleV1::LegacyCommitNamespaceInventory.artifact_token()
    );
    Ok((paths.migration_assets_dir, prefix))
}

/// Byte cap the installer applied to that asset, so the reader bounds itself
/// identically instead of picking its own number.
pub(crate) const LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_MAX_BYTES: usize =
    MAX_LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_BYTES;

/// One durable root or file an external sweep must not delete.
///
/// The role is carried beside the path because an operator reading a refusal
/// or an exclusion list needs to know WHY a path is protected; a bare path
/// list invites someone to decide one entry looks prunable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CatalogGcProtectedRootV1 {
    pub role: &'static str,
    pub path: PathBuf,
}

/// What an external sweep is allowed to do against a projects store
/// (Phase 6 plan section 10.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CatalogGcExclusionsV1 {
    /// Not a version-2 catalog store at all. Section 10.2 is a catalog-mode
    /// contract, so a bridge or absent store carries none of these roots.
    ExemptNonCatalogStore,
    /// A `FreshV2` origin. It legitimately carries no migration marker
    /// (D-011) and no rollback assets, so the marker-absence refusal
    /// deliberately does NOT fire here: refusing would make a correct store
    /// unsweepable rather than protecting anything.
    ExemptFreshOrigin,
    /// A `MigratedV1` origin whose committed marker authorized the exclusion
    /// set below.
    MarkerDriven {
        transaction_id: ProjectCatalogTransactionId,
        /// Named immutable assets the marker itself enumerates. Counted
        /// separately from `roots` so a caller can tell a marker that named
        /// nothing from one whose assets are all protected.
        named_immutable_assets: u64,
        roots: Vec<CatalogGcProtectedRootV1>,
    },
}

impl CatalogGcExclusionsV1 {
    /// Whether `path` sits inside, or exactly at, a protected root.
    ///
    /// Prefix comparison on components, not on strings: a string prefix test
    /// would treat `.../history-generations-old` as protected by
    /// `.../history-generations`, and, worse, would let a sweeper that
    /// normalized differently conclude the opposite.
    pub fn protects(&self, path: &Path) -> bool {
        let Self::MarkerDriven { roots, .. } = self else {
            return false;
        };
        roots.iter().any(|root| path.starts_with(&root.path))
    }
}

/// Plan the GC exclusion set for a projects store, or refuse to sweep
/// (Phase 6 plan section 10.2, milestone P6-C task 2).
///
/// **Why this reads the marker DIRECTLY rather than through an opened store.**
/// `open_existing` already validates the marker on every open, so a planner
/// that composed on an opened store could never observe an absent, corrupt, or
/// incomplete marker: the open would have refused first, and the refusal this
/// function exists to raise would be unreachable by construction. An exclusion
/// gate that cannot fail is not a gate. Reading the marker here also matches
/// the caller this is for: an EXTERNAL sweep has no store open, and requiring
/// one would mean the sweep either takes the store's locks or skips the check.
///
/// **Marker-driven, not a path glob.** The five roots below are the section
/// 10.2 set (transaction stage, history-rebuild stage, backup, G1, quarantine
/// - quarantine generations live under the history-generations root), but they
/// are only returned once the committed marker has authorized them, and the
/// marker's own named immutable assets are protected individually by name. A
/// glob would keep "protecting" paths after the evidence that made them
/// meaningful was gone.
///
/// Refusals all carry `error.project_catalog_migration_incomplete`, the code
/// the store already raises for marker problems; this introduces none of its
/// own.
pub fn plan_catalog_gc_exclusions(
    projects_path: &Path,
    index_root: &Path,
) -> ProjectCatalogStoreResult<CatalogGcExclusionsV1> {
    if probe_project_store_mode(projects_path)? != ProjectStoreProbe::CatalogV2 {
        return Ok(CatalogGcExclusionsV1::ExemptNonCatalogStore);
    }
    let paths = ProjectCatalogPaths::derive(projects_path)?;

    // The origin, read WITHOUT the marker binding check. A partial probe
    // rather than a full snapshot decode: this needs one field, and a full
    // decode would couple sweep planning to every future catalog field.
    let Some(raw) =
        RealCatalogStoreIo.read_regular_nofollow(&paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?
    else {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_snapshot",
            "projects store disappeared between probe and origin read",
        ));
    };
    #[derive(Deserialize)]
    struct OriginProbe {
        origin: CatalogOriginV2,
    }
    let probe: OriginProbe = serde_json::from_slice(&raw).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_snapshot",
            format!("catalog origin probe failed: {error}"),
        )
    })?;
    let transaction_id = match probe.origin {
        CatalogOriginV2::FreshV2 {} => return Ok(CatalogGcExclusionsV1::ExemptFreshOrigin),
        CatalogOriginV2::MigratedV1 { transaction_id } => transaction_id,
    };

    let incomplete = |detail: &str| {
        ProjectCatalogStoreError::new("error.project_catalog_migration_incomplete", detail)
    };
    let Some(marker_bytes) =
        RealCatalogStoreIo.read_regular_nofollow(&paths.migration_marker, MAX_MARKER_BYTES)?
    else {
        return Err(incomplete(
            "migrated catalog has no committed migration marker; refusing to sweep rather \
             than deleting rollback assets nothing can vouch for",
        ));
    };
    let marker: ProjectCatalogMigrationMarkerV1 =
        decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")
            .map_err(|error| incomplete(&format!("migration marker is corrupt: {error}")))?;
    marker.validate()?;
    if marker.transaction_id != transaction_id {
        return Err(incomplete(
            "migration marker transaction does not match catalog origin; refusing to sweep",
        ));
    }

    let accepted = AcceptedPublicationStorePaths::derive(projects_path).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_unsafe_path",
            format!("accepted-publication root cannot be derived: {error}"),
        )
    })?;
    let history_generations =
        bbox_corpus_index::index::history_generations::generations_root_for_index(index_root)
            .map_err(|error| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_unsafe_path",
                    format!("history-generations root cannot be derived: {error}"),
                )
            })?;

    let mut roots = vec![
        CatalogGcProtectedRootV1 {
            role: "transaction_stage",
            path: paths.stage_dir.clone(),
        },
        CatalogGcProtectedRootV1 {
            role: "catalog_backup",
            path: paths.backup_dir.clone(),
        },
        CatalogGcProtectedRootV1 {
            role: "migration_immutable_assets",
            path: paths.migration_assets_dir.clone(),
        },
        CatalogGcProtectedRootV1 {
            role: "accepted_publication_generations",
            path: accepted.generations().to_path_buf(),
        },
        // Both the history-rebuild stage and the quarantine generations live
        // here: quarantine generations are `rhq_`-id'd entries in the same
        // store, and the rebuild manifest is their only durable owner.
        CatalogGcProtectedRootV1 {
            role: "history_generations",
            path: history_generations,
        },
    ];
    // The marker's OWN named inventory, protected by name. This is what makes
    // the exclusion marker-driven: these files are named by the committed
    // evidence, not discovered by walking a directory.
    for asset in &marker.immutable_assets {
        roots.push(CatalogGcProtectedRootV1 {
            role: "migration_immutable_asset",
            path: paths
                .migration_assets_dir
                .join(asset.validated_name.as_str()),
        });
    }

    Ok(CatalogGcExclusionsV1::MarkerDriven {
        transaction_id,
        named_immutable_assets: marker.immutable_assets.len() as u64,
        roots,
    })
}

impl ProjectCatalogStore {
    /// Open an already initialized strict v2 pair.
    ///
    /// Two missing snapshots are not interpreted as an empty store here.
    pub fn open_existing(projects_path: impl Into<PathBuf>) -> ProjectCatalogStoreResult<Self> {
        Self::open_existing_with_io(projects_path.into(), Arc::new(RealCatalogStoreIo))
    }

    /// Initialize an explicitly new store at epoch one.
    ///
    /// Initialization requires exclusive process-lifetime authority and
    /// rejects any pre-existing catalog or attachment image.
    pub fn initialize_empty(projects_path: impl Into<PathBuf>) -> ProjectCatalogStoreResult<Self> {
        Self::initialize_empty_with_io(projects_path.into(), Arc::new(RealCatalogStoreIo))
    }

    pub fn snapshot(&self) -> ProjectCatalogStoreResult<Arc<ProjectCatalogState>> {
        match &*self.current.read() {
            PublishedStoreState::Ready(state) => Ok(state.clone()),
            PublishedStoreState::Poisoned(error) => Err(error.clone()),
        }
    }

    /// The publisher-disposition evidence the migration retained in its marker.
    ///
    /// The durable-backfill preflight needs the migration's reviewed publisher
    /// dispositions, and the marker is their only durable source: the CLI
    /// cannot supply them (the migration resolution artifact is not on the
    /// backfill surface per FD-3, and receipts carry only a count).
    ///
    /// Deliberately narrow. It returns the evidence and nothing else of the
    /// marker, so this does not become a general marker getter.
    ///
    /// An ABSENT or unreadable marker is a typed refusal, never an empty set.
    /// An empty set verifies vacuously and would report a clean publisher check
    /// on a store whose marker is missing or corrupt, which is the exact silent
    /// pass the backfill's publisher verification exists to prevent.
    ///
    /// The journal binding is verified on this read like every other, so a
    /// marker disagreeing with its transaction journal refuses rather than
    /// being trusted. The mutation lock is acquired HERE rather than assumed:
    /// the store released it at the end of `open_existing`, and the
    /// `_locked` journal reads below require it.
    pub(crate) fn migration_publisher_dispositions(
        &self,
    ) -> ProjectCatalogStoreResult<Vec<PublisherDispositionEvidenceV1>> {
        let _mutation_lock = self
            .owner
            .io
            .acquire_mutation_lock(&self.owner.paths.catalog)?;
        let marker_bytes = self
            .owner
            .io
            .read_regular_nofollow(&self.owner.paths.migration_marker, MAX_MARKER_BYTES)?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migrated catalog lacks its retained migration marker",
                )
            })?;
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")?;
        let journal = self
            .owner
            .committed_migration_journal_for_marker_locked(&marker)?;
        verify_migration_marker_journal_binding(&marker, &marker_bytes, &journal)?;
        Ok(marker.publisher_dispositions)
    }

    /// Force snapshot reads to fail, returning the state that was current
    /// so a caller can restore it.
    ///
    /// Test-only, and deliberately narrow: it changes snapshot readability
    /// and nothing else. No durable byte moves, the transaction owner is
    /// untouched, and the poisoned arm it installs is the same one a failed
    /// recovery installs in production. That is what makes it worth having
    /// rather than a mock: a consumer proved to degrade correctly here is
    /// proved against the real unreadable-pair state.
    ///
    /// `None` when the store was already poisoned.
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn poison_for_test(&self, detail: &str) -> Option<Arc<ProjectCatalogState>> {
        let mut current = self.current.write();
        let previous = match &*current {
            PublishedStoreState::Ready(state) => Some(state.clone()),
            PublishedStoreState::Poisoned(_) => None,
        };
        *current = PublishedStoreState::Poisoned(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_snapshot",
            detail,
        ));
        previous
    }

    /// Restore a state captured by [`Self::poison_for_test`].
    #[cfg(any(test, feature = "test-support"))]
    #[doc(hidden)]
    pub fn unpoison_for_test(&self, state: Arc<ProjectCatalogState>) {
        *self.current.write() = PublishedStoreState::Ready(state);
    }

    /// Returns a clone of the post-commit observer handle (section 9.4).
    ///
    /// The observer accumulates `CatalogCommittedEvent`s emitted after
    /// each successful `transact`. Callers drain events and map affected
    /// project ids to reconciler actions.
    pub fn commit_observer(&self) -> CatalogCommitObserver {
        self.commit_observer.clone()
    }

    /// Mutate a complete catalog and attachment post-image under epoch CAS.
    ///
    /// The closure runs on private clones before the mutation lock is held.
    /// It cannot change versions, epochs, or catalog origin.
    pub fn transact(
        &self,
        expected_epoch: u64,
        build: impl FnOnce(
            &mut CatalogSnapshotV2,
            &mut AttachmentSnapshotV1,
        ) -> ProjectCatalogStoreResult<()>,
    ) -> ProjectCatalogStoreResult<ProjectCatalogCommit> {
        let base = self.snapshot()?;
        if base.epoch != expected_epoch {
            return Err(stale_epoch(expected_epoch, base.epoch));
        }

        // Capture the set of project ids present before the mutation so
        // the post-commit observer can compute the changed set
        // (section 9.4: emit changed project ids after durable pair
        // publication and lock release). We keep the base catalog's
        // project map AND attachment map for per-entry comparison after
        // the build closure.
        let old_projects = base.catalog.projects.clone();
        let old_attachments = base.attachments.clone();

        let mut catalog = (*base.catalog).clone();
        let mut attachments = (*base.attachments).clone();
        let invariant_version = (catalog.version, attachments.version);
        let invariant_epoch = (catalog.epoch, attachments.epoch);
        let invariant_origin = catalog.origin.clone();
        build(&mut catalog, &mut attachments)?;
        if (catalog.version, attachments.version) != invariant_version
            || (catalog.epoch, attachments.epoch) != invariant_epoch
            || catalog.origin != invariant_origin
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_owner_field_mutation",
                "transaction closure changed owner-controlled fields",
            ));
        }
        let new_project_ids: BTreeSet<String> = catalog
            .projects
            .keys()
            .map(|k| k.as_str().to_string())
            .collect();
        let old_project_ids: BTreeSet<String> = old_projects
            .keys()
            .map(|k| k.as_str().to_string())
            .collect();
        // Compute changed project ids before catalog moves into candidate
        // (section 9.4). A project id is "changed" if it was added,
        // removed, or its entry content differs between old and new
        // catalog snapshots, OR if its attachment entry differs between
        // old and new attachment snapshots (attachment-only operations
        // must also emit changed ids).
        let new_attachment_project_ids: BTreeSet<String> = attachments
            .attachments
            .values()
            .map(|a| a.project_id.as_str().to_string())
            .collect();
        let old_attachment_project_ids: BTreeSet<String> = old_attachments
            .attachments
            .values()
            .map(|a| a.project_id.as_str().to_string())
            .collect();
        let changed_project_ids: BTreeSet<String> = new_project_ids
            .iter()
            .chain(old_project_ids.iter())
            .chain(new_attachment_project_ids.iter())
            .chain(old_attachment_project_ids.iter())
            .filter(|pid| {
                let old_entry = old_projects
                    .iter()
                    .find(|(k, _)| k.as_str() == pid.as_str())
                    .map(|(_, v)| v);
                let new_entry = catalog
                    .projects
                    .iter()
                    .find(|(k, _)| k.as_str() == pid.as_str())
                    .map(|(_, v)| v);
                let catalog_changed = match (old_entry, new_entry) {
                    (None, Some(_)) => true,
                    (Some(_), None) => true,
                    (Some(old), Some(new)) => old != new,
                    (None, None) => false,
                };
                if catalog_changed {
                    return true;
                }
                // R2F6: compare the COMPLETE set of attachments for this
                // project, not just the first one found. A project with
                // multiple attachments where only the second changed must
                // be detected.
                let old_atts: BTreeMap<&str, &CheckoutAttachment> = old_attachments
                    .attachments
                    .values()
                    .filter(|a| a.project_id.as_str() == pid.as_str())
                    .map(|a| (a.attachment_id.as_str(), a))
                    .collect();
                let new_atts: BTreeMap<&str, &CheckoutAttachment> = attachments
                    .attachments
                    .values()
                    .filter(|a| a.project_id.as_str() == pid.as_str())
                    .map(|a| (a.attachment_id.as_str(), a))
                    .collect();
                if old_atts.len() != new_atts.len() {
                    return true;
                }
                for (key, old_val) in &old_atts {
                    match new_atts.get(*key) {
                        Some(new_val) if old_val != new_val => return true,
                        None => return true,
                        _ => {}
                    }
                }
                // R2F6: also compare default_attachments selection.
                let pid_parsed = ProjectId::parse(pid.as_str()).ok();
                if let Some(pid_key) = &pid_parsed {
                    let old_default = old_attachments.default_attachments.get(pid_key);
                    let new_default = attachments.default_attachments.get(pid_key);
                    if old_default != new_default {
                        return true;
                    }
                }
                false
            })
            .cloned()
            .collect();
        let new_epoch = expected_epoch.checked_add(1).ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_epoch_overflow",
                "catalog epoch cannot be incremented",
            )
        })?;
        catalog.epoch = new_epoch;
        attachments.epoch = new_epoch;
        let candidate = PreparedPair::new(catalog, attachments)?;

        let _mutation_lock = self
            .owner
            .io
            .acquire_mutation_lock(&self.owner.paths.catalog)?;
        let _auxiliary_locks = self.owner.acquire_auxiliary_locks()?;
        let locked_result = (|| {
            self.owner.recover_locked().map_err(|error| (error, None))?;
            let installed = self
                .owner
                .read_strict_pair_locked()
                .map_err(|error| (error, None))?;
            if installed.epoch != expected_epoch
                || installed.catalog_sha256 != base.catalog_sha256
                || installed.attachments_sha256 != base.attachments_sha256
            {
                return Err((
                    stale_epoch(expected_epoch, installed.epoch),
                    Some(installed),
                ));
            }
            self.owner
                .commit_regular_pair_locked(Some(&installed), &candidate)
                .map_err(|error| (error, None))?;
            Ok(())
        })();
        if let Err((error, known_state)) = locked_result {
            if let Some(state) = known_state {
                *self.current.write() = PublishedStoreState::Ready(Arc::new(state));
            } else {
                self.reconcile_after_error_locked(&error);
            }
            return Err(error);
        }
        let committed = Arc::new(candidate.into_state());
        let result = ProjectCatalogCommit {
            epoch: committed.epoch,
            catalog_sha256: committed.catalog_sha256.to_string(),
            attachments_sha256: committed.attachments_sha256.to_string(),
        };
        *self.current.write() = PublishedStoreState::Ready(committed);
        // The mutation lock and auxiliary locks (_mutation_lock,
        // _auxiliary_locks) are released when this function returns.
        // Move the observer push past them so the emission happens
        // AFTER durable pair publication AND after lock release
        // (section 9.4).
        let observer = self.commit_observer.clone();
        let observer_event = CatalogCommittedEvent {
            epoch: result.epoch,
            changed_project_ids,
        };
        // Drop the locks explicitly so the observer emission is
        // strictly post-release.
        drop(_auxiliary_locks);
        drop(_mutation_lock);
        observer.push(observer_event);
        Ok(result)
    }

    fn reconcile_after_error_locked(&self, transaction_error: &ProjectCatalogStoreError) {
        let reconciled = self
            .owner
            .recover_locked()
            .and_then(|()| self.owner.read_strict_pair_locked());
        *self.current.write() = match reconciled {
            Ok(state) => PublishedStoreState::Ready(Arc::new(state)),
            Err(recovery_error) => PublishedStoreState::Poisoned(ProjectCatalogStoreError::new(
                "error.project_catalog_store_poisoned",
                format!(
                    "transaction failed with {}; reconciliation failed with {}",
                    transaction_error.code(),
                    recovery_error.code()
                ),
            )),
        };
    }

    fn open_existing_with_io(
        projects_path: PathBuf,
        io: Arc<dyn CatalogStoreIo>,
    ) -> ProjectCatalogStoreResult<Self> {
        Self::open_existing_with_registry_and_io(projects_path, ParticipantRegistry::Regular, io)
    }

    #[allow(dead_code)] // P1-B seam consumed by P1-C runtime startup.
    pub(crate) fn open_existing_after_migration(
        projects_path: PathBuf,
        registry: MigrationParticipantRegistry,
    ) -> ProjectCatalogStoreResult<Self> {
        Self::open_existing_after_migration_classified(projects_path, registry)
            .map(|opened| opened.store)
            .map_err(|failure| failure.error)
    }

    pub(crate) fn open_existing_after_migration_classified(
        projects_path: PathBuf,
        registry: MigrationParticipantRegistry,
    ) -> Result<MigrationStoreOpenV1, MigrationStoreOpenFailureV1> {
        let registry = registry.validate().map_err(open_pre_entry_failure)?;
        if registry.catalog_path != projects_path {
            return Err(open_pre_entry_failure(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "migration registry is bound to a different project catalog path",
            )));
        }
        Self::open_existing_after_migration_classified_with_io(
            projects_path,
            ParticipantRegistry::Migration(Arc::new(registry)),
            Arc::new(RealCatalogStoreIo),
        )
    }

    fn open_existing_after_migration_classified_with_io(
        projects_path: PathBuf,
        registry: ParticipantRegistry,
        io: Arc<dyn CatalogStoreIo>,
    ) -> Result<MigrationStoreOpenV1, MigrationStoreOpenFailureV1> {
        let paths = ProjectCatalogPaths::derive(&projects_path).map_err(open_pre_entry_failure)?;
        let lifetime_lock = Arc::new(
            ProjectCatalogMigrationLock::acquire_shared(&paths.catalog)
                .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))
                .map_err(open_pre_entry_failure)?,
        );
        let owner = ProjectCatalogTransactionOwner {
            paths,
            registry,
            io,
        };
        let _mutation_lock = owner
            .io
            .acquire_mutation_lock(&owner.paths.catalog)
            .map_err(open_pre_entry_failure)?;
        let _auxiliary_locks = owner
            .acquire_auxiliary_locks()
            .map_err(open_pre_entry_failure)?;
        let before = owner
            .read_journal_locked()
            .map_err(open_recovery_uncertain_failure)?;
        owner
            .recover_locked()
            .map_err(open_recovery_uncertain_failure)?;
        let disposition = match before {
            None => MigrationMutationDispositionV1::NoDurableMutation,
            Some(journal) if journal.kind == TransactionKindV1::V1Migration => {
                let after = owner
                    .read_journal_locked()
                    .map_err(open_recovery_uncertain_failure)?;
                recovered_journal_disposition(after.as_ref()).ok_or_else(|| {
                    open_recovery_uncertain_failure(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "migration recovery did not reach a terminal journal outcome",
                    ))
                })?
            }
            Some(_) => {
                let marker_bytes = owner
                    .io
                    .read_regular_nofollow(&owner.paths.migration_marker, MAX_MARKER_BYTES)
                    .map_err(open_recovery_uncertain_failure)?
                    .ok_or_else(|| {
                        open_recovery_uncertain_failure(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "regular catalog history lacks its retained migration marker",
                        ))
                    })?;
                let marker: ProjectCatalogMigrationMarkerV1 =
                    decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")
                        .map_err(open_recovery_uncertain_failure)?;
                let migration = owner
                    .committed_migration_journal_for_marker_locked(&marker)
                    .map_err(open_recovery_uncertain_failure)?;
                verify_migration_marker_journal_binding(&marker, &marker_bytes, &migration)
                    .map_err(open_recovery_uncertain_failure)?;
                recovered_journal_disposition(Some(&migration)).ok_or_else(|| {
                    open_recovery_uncertain_failure(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "retained migration journal is not committed",
                    ))
                })?
            }
        };
        let current = Arc::new(
            owner
                .read_strict_pair_locked()
                .map_err(|error| MigrationStoreOpenFailureV1 { error, disposition })?,
        );
        drop(_mutation_lock);
        Ok(MigrationStoreOpenV1 {
            store: Self {
                owner,
                current: RwLock::new(PublishedStoreState::Ready(current)),
                _lifetime_lock: lifetime_lock,
                commit_observer: CatalogCommitObserver::new(),
            },
            disposition,
        })
    }

    /// Returns path-redacted committed migration evidence after a fresh
    /// marker, journal, participant, immutable-asset, and registry check.
    #[allow(dead_code)] // P1-C verification and apply receipts consume this seam.
    pub(crate) fn migration_artifact_identity(
        &self,
    ) -> ProjectCatalogStoreResult<MigrationArtifactIdentityV1> {
        let _mutation_lock = self
            .owner
            .io
            .acquire_mutation_lock(&self.owner.paths.catalog)?;
        let _auxiliary_locks = self.owner.acquire_auxiliary_locks()?;
        let marker_bytes = self
            .owner
            .io
            .read_regular_nofollow(&self.owner.paths.migration_marker, MAX_MARKER_BYTES)?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migration verification lacks its retained marker",
                )
            })?;
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")?;
        let migration_install_is_current =
            self.owner.read_journal_locked()?.is_some_and(|active| {
                active.kind == TransactionKindV1::V1Migration
                    && active.transaction_id == marker.transaction_id
            });
        let journal = self
            .owner
            .committed_migration_journal_for_marker_locked(&marker)?;
        verify_migration_marker_journal_binding(&marker, &marker_bytes, &journal)?;
        self.owner.verify_current_migration_state(&journal)?;
        migration_artifact_identity_from_journal(
            &journal,
            marker,
            sha256(&marker_bytes),
            migration_install_is_current,
        )
    }

    fn open_existing_with_registry_and_io(
        projects_path: PathBuf,
        registry: ParticipantRegistry,
        io: Arc<dyn CatalogStoreIo>,
    ) -> ProjectCatalogStoreResult<Self> {
        let paths = ProjectCatalogPaths::derive(&projects_path)?;
        let lifetime_lock = Arc::new(
            ProjectCatalogMigrationLock::acquire_shared(&paths.catalog)
                .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))?,
        );
        let owner = ProjectCatalogTransactionOwner {
            paths,
            registry,
            io,
        };
        let _mutation_lock = owner.io.acquire_mutation_lock(&owner.paths.catalog)?;
        let _auxiliary_locks = owner.acquire_auxiliary_locks()?;
        owner.recover_locked()?;
        let current = Arc::new(owner.read_strict_pair_locked()?);
        drop(_mutation_lock);
        Ok(Self {
            owner,
            current: RwLock::new(PublishedStoreState::Ready(current)),
            _lifetime_lock: lifetime_lock,
            commit_observer: CatalogCommitObserver::new(),
        })
    }

    fn initialize_empty_with_io(
        projects_path: PathBuf,
        io: Arc<dyn CatalogStoreIo>,
    ) -> ProjectCatalogStoreResult<Self> {
        let paths = ProjectCatalogPaths::derive(&projects_path)?;
        let exclusive = ProjectCatalogMigrationLock::try_acquire_exclusive(&paths.catalog)
            .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_lifetime_lock_busy",
                    "a compatible daemon or preflight still holds the lifetime lock",
                )
            })?;
        let owner = ProjectCatalogTransactionOwner {
            paths: paths.clone(),
            registry: ParticipantRegistry::Regular,
            io: io.clone(),
        };
        let _mutation_lock = owner.io.acquire_mutation_lock(&owner.paths.catalog)?;
        owner.recover_locked()?;
        match (
            owner
                .io
                .read_regular_nofollow(&paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?,
            owner
                .io
                .read_regular_nofollow(&paths.attachments, MAX_PROJECT_CATALOG_BYTES)?,
            owner
                .io
                .read_regular_nofollow(&paths.migration_marker, MAX_MARKER_BYTES)?,
        ) {
            (None, None, None) => {}
            _ => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_already_initialized",
                    "new-store initialization found existing strict state",
                ));
            }
        }
        let empty = PreparedPair::new(
            CatalogSnapshotV2::empty(1).map_err(contract_error)?,
            AttachmentSnapshotV1::empty(1).map_err(contract_error)?,
        )?;
        owner.commit_regular_pair_locked(None, &empty)?;
        let current = Arc::new(owner.read_strict_pair_locked()?);
        drop(_mutation_lock);
        let lifetime_lock = Arc::new(
            exclusive
                .downgrade_to_shared()
                .map_err(|error| io_error("downgrade lifetime lock for", &paths.catalog, error))?,
        );
        Ok(Self {
            owner,
            current: RwLock::new(PublishedStoreState::Ready(current)),
            _lifetime_lock: lifetime_lock,
            commit_observer: CatalogCommitObserver::new(),
        })
    }
}

fn open_pre_entry_failure(error: ProjectCatalogStoreError) -> MigrationStoreOpenFailureV1 {
    MigrationStoreOpenFailureV1 {
        error,
        disposition: MigrationMutationDispositionV1::NoDurableMutation,
    }
}

fn open_recovery_uncertain_failure(error: ProjectCatalogStoreError) -> MigrationStoreOpenFailureV1 {
    MigrationStoreOpenFailureV1 {
        error,
        disposition: MigrationMutationDispositionV1::RetryExactPlanRequired,
    }
}

fn bootstrap_retry_failure(error: ProjectCatalogStoreError) -> MigrationBootstrapFailureV1 {
    bootstrap_journal_failure(
        error,
        MigrationMutationDispositionV1::RetryExactPlanRequired,
    )
}

fn bootstrap_pre_entry_failure(error: ProjectCatalogStoreError) -> MigrationBootstrapFailureV1 {
    bootstrap_journal_failure(error, MigrationMutationDispositionV1::NoDurableMutation)
}

fn bootstrap_journal_failure(
    error: ProjectCatalogStoreError,
    disposition: MigrationMutationDispositionV1,
) -> MigrationBootstrapFailureV1 {
    MigrationBootstrapFailureV1 { error, disposition }
}

fn recovered_journal_disposition(
    journal: Option<&ProjectCatalogTransactionJournalV1>,
) -> Option<MigrationMutationDispositionV1> {
    match journal.map(|journal| (&journal.kind, &journal.state, &journal.outcome)) {
        Some((
            TransactionKindV1::V1Migration,
            TransactionStateV1::Committed,
            Some(TransactionOutcomeV1::Committed),
        )) => Some(MigrationMutationDispositionV1::RecoveredToCommittedState),
        Some((
            TransactionKindV1::V1Migration,
            TransactionStateV1::Committed,
            Some(TransactionOutcomeV1::RolledBack),
        )) => Some(MigrationMutationDispositionV1::RecoveredToOldState),
        _ => None,
    }
}

fn stale_epoch(expected: u64, actual: u64) -> ProjectCatalogStoreError {
    ProjectCatalogStoreError::new(
        "error.project_catalog_stale_epoch",
        format!("expected epoch {expected}, found {actual}"),
    )
}

#[derive(Debug, Clone)]
struct ProjectCatalogPaths {
    catalog: PathBuf,
    attachments: PathBuf,
    journal: PathBuf,
    migration_marker: PathBuf,
    migration_receipt: PathBuf,
    migration_assets_dir: PathBuf,
    stage_dir: PathBuf,
    backup_dir: PathBuf,
    mutation_lock: PathBuf,
    lifetime_lock: PathBuf,
}

impl ProjectCatalogPaths {
    fn derive(projects_path: &Path) -> ProjectCatalogStoreResult<Self> {
        if !projects_path.is_absolute() {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_unsafe_path",
                "configured projects path must be absolute",
            ));
        }
        let parent = projects_path.parent().ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_unsafe_path",
                "configured projects path has no parent",
            )
        })?;
        let basename = projects_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| valid_basename(name))
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_unsafe_path",
                    "configured projects filename is unsafe",
                )
            })?;
        let reserved = [
            "project-attachments.json",
            "project-catalog-transaction.json",
            "project-catalog-migration.json",
            "project-catalog-migration-receipt.json",
            "project-catalog-migration-assets",
            "project-catalog-stage",
            "project-catalog-backups",
            "project-catalog-migration.lock",
        ];
        if reserved.contains(&basename) {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_path_collision",
                "configured projects filename collides with a fixed sibling",
            ));
        }

        let paths = Self {
            catalog: projects_path.to_path_buf(),
            attachments: parent.join("project-attachments.json"),
            journal: parent.join("project-catalog-transaction.json"),
            migration_marker: parent.join("project-catalog-migration.json"),
            migration_receipt: parent.join("project-catalog-migration-receipt.json"),
            migration_assets_dir: parent.join("project-catalog-migration-assets"),
            stage_dir: parent.join("project-catalog-stage"),
            backup_dir: parent.join("project-catalog-backups"),
            mutation_lock: canonical_store_lock_path(projects_path),
            lifetime_lock: project_catalog_migration_lock_path(projects_path),
        };
        let unique = [
            &paths.catalog,
            &paths.attachments,
            &paths.journal,
            &paths.migration_marker,
            &paths.migration_receipt,
            &paths.migration_assets_dir,
            &paths.stage_dir,
            &paths.backup_dir,
            &paths.mutation_lock,
            &paths.lifetime_lock,
        ]
        .into_iter()
        .collect::<BTreeSet<_>>();
        if unique.len() != 10 {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_path_collision",
                "derived project catalog paths are not unique",
            ));
        }
        Ok(paths)
    }
}

fn valid_basename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && !matches!(name, "." | "..")
        && !name.contains(['/', '\\'])
        && !name
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
}

#[derive(Clone)]
struct ProjectCatalogTransactionOwner {
    paths: ProjectCatalogPaths,
    registry: ParticipantRegistry,
    io: Arc<dyn CatalogStoreIo>,
}

#[derive(Clone)]
enum ParticipantRegistry {
    Regular,
    #[allow(dead_code)] // P1-B seam consumed by the P1-C apply engine.
    Migration(Arc<MigrationParticipantRegistry>),
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B defines the closed registry before P1-C populates it.
pub(crate) struct MigrationParticipantRegistry {
    catalog_path: PathBuf,
    code_source_paths: CodeSourceStorePaths,
    accepted_publication_paths: AcceptedPublicationStorePaths,
    legacy_publisher_ref_source: PathBuf,
    catalog_immutable_root: PathBuf,
    checkout_identity_markers: std::collections::BTreeMap<String, PathBuf>,
    code_source_limits: StoreLimits,
}

pub(crate) enum MigrationCheckoutRegistryBootstrapV1 {
    FreshLegacyNotInstalled,
    RolledBackNotInstalled {
        disposition: MigrationMutationDispositionV1,
    },
    RequiresRegistry(MigrationCheckoutRegistryBootstrapSessionV1),
}

pub(crate) struct MigrationCheckoutRegistryBootstrapSessionV1 {
    owner: ProjectCatalogTransactionOwner,
    journal: ProjectCatalogTransactionJournalV1,
    disposition: MigrationMutationDispositionV1,
    lifetime_lock: Arc<ProjectCatalogMigrationLock>,
    mutation_lock: StoreLockGuard,
}

pub(crate) struct MigrationCheckoutRegistryBoundSessionV1 {
    owner: ProjectCatalogTransactionOwner,
    base_registry: MigrationParticipantRegistry,
    journal: ProjectCatalogTransactionJournalV1,
    disposition: MigrationMutationDispositionV1,
    lifetime_lock: Arc<ProjectCatalogMigrationLock>,
    mutation_lock: StoreLockGuard,
    auxiliary_locks: Vec<StoreLockGuard>,
}

impl fmt::Debug for MigrationCheckoutRegistryBootstrapV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FreshLegacyNotInstalled => formatter.write_str("FreshLegacyNotInstalled"),
            Self::RolledBackNotInstalled { disposition } => formatter
                .debug_struct("RolledBackNotInstalled")
                .field("disposition", disposition)
                .finish(),
            Self::RequiresRegistry(session) => formatter
                .debug_struct("RequiresRegistry")
                .field("disposition", &session.disposition)
                .finish_non_exhaustive(),
        }
    }
}

#[allow(dead_code)] // P1-B defines the closed registry before P1-C populates it.
impl MigrationParticipantRegistry {
    pub(crate) fn new(
        projects_path: &Path,
        code_source_root: PathBuf,
        legacy_publisher_ref_source: PathBuf,
        code_source_limits: StoreLimits,
    ) -> ProjectCatalogStoreResult<Self> {
        let paths = ProjectCatalogPaths::derive(projects_path)?;
        let code_source_paths = CodeSourceStorePaths::new(code_source_root).map_err(|error| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                error.to_string(),
            )
        })?;
        let accepted_publication_paths = AcceptedPublicationStorePaths::derive(projects_path)
            .map_err(|error| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_registry",
                    error.to_string(),
                )
            })?;
        Self {
            catalog_path: paths.catalog,
            code_source_paths,
            accepted_publication_paths,
            legacy_publisher_ref_source,
            catalog_immutable_root: paths.migration_assets_dir.clone(),
            checkout_identity_markers: std::collections::BTreeMap::new(),
            code_source_limits,
        }
        .validate()
    }

    pub(crate) fn register_checkout_identity(
        &mut self,
        observation_id: String,
        checkout_root: PathBuf,
    ) -> ProjectCatalogStoreResult<()> {
        if observation_id.is_empty()
            || observation_id.len() > 256
            || observation_id.chars().any(char::is_control)
            || !safe_absolute_path(&checkout_root)
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "checkout identity observation or root is invalid",
            ));
        }
        let target = checkout_root
            .join(".bbox")
            .join("local")
            .join("checkout-id");
        if self
            .checkout_identity_markers
            .insert(observation_id, target)
            .is_some()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "checkout identity observation is duplicated",
            ));
        }
        Ok(())
    }

    pub(crate) fn validate(self) -> ProjectCatalogStoreResult<Self> {
        let paths = ProjectCatalogPaths::derive(&self.catalog_path)?;
        let distinct_checkout_targets = self
            .checkout_identity_markers
            .values()
            .collect::<BTreeSet<_>>();
        let auxiliary_paths = self.auxiliary_store_paths();
        let control_paths = vec![
            paths.catalog.clone(),
            paths.attachments.clone(),
            paths.journal.clone(),
            paths.migration_marker.clone(),
            paths.migration_receipt.clone(),
            paths.migration_assets_dir.clone(),
            paths.stage_dir.clone(),
            paths.backup_dir.clone(),
            paths.mutation_lock.clone(),
            paths.lifetime_lock.clone(),
        ];
        let mut lock_paths = Vec::new();
        for path in &auxiliary_paths {
            lock_paths.push(canonical_store_lock_path(path));
        }
        let mut all_fixed_paths = control_paths.clone();
        all_fixed_paths.extend(auxiliary_paths.iter().cloned());
        all_fixed_paths.extend(lock_paths);
        let distinct_fixed_paths = all_fixed_paths.iter().collect::<BTreeSet<_>>();
        let code_source_root = self.code_source_paths.root();
        let accepted_publication_root = self.accepted_publication_paths.root();
        let root_collides = control_paths.iter().any(|path| {
            paths_overlap(path, code_source_root)
                || paths_overlap(path, accepted_publication_root)
                || paths_overlap(path, &self.legacy_publisher_ref_source)
        }) || paths_overlap(code_source_root, accepted_publication_root)
            || paths_overlap(code_source_root, &self.legacy_publisher_ref_source)
            || paths_overlap(accepted_publication_root, &self.legacy_publisher_ref_source);
        let checkout_collides = self.checkout_identity_markers.values().any(|target| {
            control_paths.iter().any(|path| paths_overlap(path, target))
                || paths_overlap(code_source_root, target)
                || paths_overlap(accepted_publication_root, target)
                || paths_overlap(&self.legacy_publisher_ref_source, target)
        });
        if !safe_absolute_path(&self.catalog_path)
            || !safe_absolute_path(self.code_source_paths.root())
            || !safe_absolute_path(self.accepted_publication_paths.anchor())
            || !safe_absolute_path(self.accepted_publication_paths.root())
            || !safe_absolute_path(&self.legacy_publisher_ref_source)
            || !safe_absolute_path(&self.catalog_immutable_root)
            || self
                .checkout_identity_markers
                .values()
                .any(|path| !safe_absolute_path(path))
            || distinct_checkout_targets.len() != self.checkout_identity_markers.len()
            || distinct_fixed_paths.len() != all_fixed_paths.len()
            || root_collides
            || checkout_collides
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "migration registry contains an unsafe, duplicated, or colliding control path",
            ));
        }
        Ok(self)
    }

    fn participant_target(&self, role: &ParticipantRoleV1) -> Option<PathBuf> {
        match role {
            ParticipantRoleV1::EffectiveSourceManifest => Some(self.code_source_paths.anchor()),
            ParticipantRoleV1::Activation { project_id } => {
                Some(self.code_source_paths.activation(project_id))
            }
            ParticipantRoleV1::StoredGenerationMetadata {
                published_scope,
                generation_id,
                ..
            } => self
                .code_source_paths
                .generation_metadata(published_scope, generation_id.as_str())
                .ok(),
            ParticipantRoleV1::CollisionRetirement { project_id } => Some(
                self.code_source_paths
                    .collision_retirement_pending(project_id),
            ),
            ParticipantRoleV1::AcceptedPublicationPointer { project_id } => {
                Some(self.accepted_publication_paths.pointer(project_id))
            }
            ParticipantRoleV1::Catalog
            | ParticipantRoleV1::Attachments
            | ParticipantRoleV1::MigrationMarker => None,
        }
    }

    fn immutable_target(
        &self,
        role: &ImmutableAssetRoleV1,
        validated_name: &ValidatedBasename,
    ) -> PathBuf {
        match role {
            ImmutableAssetRoleV1::LegacyProjectStoreBackup
            | ImmutableAssetRoleV1::LegacyPublisherRefBackup
            | ImmutableAssetRoleV1::LegacyCommitNamespaceInventory => {
                self.catalog_immutable_root.join(validated_name.as_str())
            }
            ImmutableAssetRoleV1::AcceptedPublicationGeneration {
                project_id,
                generation_id,
            } => self
                .accepted_publication_paths
                .generation(project_id, generation_id),
            ImmutableAssetRoleV1::CollectedGenerationManifest {
                published_scope,
                generation_id,
            } => self
                .code_source_paths
                .generation_manifest(published_scope, generation_id.as_str())
                .expect("validated immutable collected manifest role"),
        }
    }

    fn checkout_identity_target(&self, observation_id: &str) -> Option<PathBuf> {
        self.checkout_identity_markers.get(observation_id).cloned()
    }

    fn checkout_root(&self, observation_id: &str) -> Option<PathBuf> {
        self.checkout_identity_markers
            .get(observation_id)
            .and_then(|target| target.parent())
            .and_then(Path::parent)
            .and_then(Path::parent)
            .map(Path::to_path_buf)
    }

    fn auxiliary_store_paths(&self) -> Vec<PathBuf> {
        let mut paths = vec![
            self.code_source_paths.anchor(),
            self.accepted_publication_paths.anchor().to_path_buf(),
            self.legacy_publisher_ref_source.clone(),
        ];
        paths.sort();
        paths.dedup();
        paths
    }
}

pub(crate) fn begin_migration_checkout_registry_bootstrap(
    projects_path: &Path,
) -> Result<MigrationCheckoutRegistryBootstrapV1, MigrationBootstrapFailureV1> {
    begin_migration_checkout_registry_bootstrap_with_io(projects_path, Arc::new(RealCatalogStoreIo))
}

fn begin_migration_checkout_registry_bootstrap_with_io(
    projects_path: &Path,
    io: Arc<dyn CatalogStoreIo>,
) -> Result<MigrationCheckoutRegistryBootstrapV1, MigrationBootstrapFailureV1> {
    let paths = ProjectCatalogPaths::derive(projects_path).map_err(bootstrap_pre_entry_failure)?;
    let lifetime_lock = Arc::new(
        ProjectCatalogMigrationLock::acquire_shared(&paths.catalog)
            .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))
            .map_err(bootstrap_pre_entry_failure)?,
    );
    let owner = ProjectCatalogTransactionOwner {
        paths,
        registry: ParticipantRegistry::Regular,
        io,
    };
    let mutation_lock = owner
        .io
        .acquire_mutation_lock(&owner.paths.catalog)
        .map_err(bootstrap_pre_entry_failure)?;
    let Some(mut journal) = owner
        .read_journal_locked()
        .map_err(bootstrap_retry_failure)?
    else {
        let catalog_bytes = owner
            .io
            .read_regular_nofollow(&owner.paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)
            .map_err(bootstrap_retry_failure)?;
        if catalog_bytes
            .as_deref()
            .is_some_and(|bytes| decode_legacy_project_store(bytes).is_err())
            || owner
                .io
                .read_regular_nofollow(&owner.paths.attachments, MAX_PROJECT_CATALOG_BYTES)
                .map_err(bootstrap_retry_failure)?
                .is_some()
            || owner
                .io
                .read_regular_nofollow(&owner.paths.migration_marker, MAX_MARKER_BYTES)
                .map_err(bootstrap_retry_failure)?
                .is_some()
            || path_exists_nofollow(&owner.paths.migration_receipt)
                .map_err(bootstrap_retry_failure)?
            || path_exists_nofollow(&owner.paths.migration_assets_dir)
                .map_err(bootstrap_retry_failure)?
            || path_exists_nofollow(&owner.paths.stage_dir).map_err(bootstrap_retry_failure)?
            || path_exists_nofollow(&owner.paths.backup_dir).map_err(bootstrap_retry_failure)?
        {
            return Err(bootstrap_retry_failure(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "journal-free migration bootstrap found partial or v2 migration state",
            )));
        }
        return Ok(MigrationCheckoutRegistryBootstrapV1::FreshLegacyNotInstalled);
    };
    if journal.kind == TransactionKindV1::RegularPair {
        owner.recover_locked().map_err(bootstrap_retry_failure)?;
        let marker_bytes = owner
            .io
            .read_regular_nofollow(&owner.paths.migration_marker, MAX_MARKER_BYTES)
            .map_err(bootstrap_retry_failure)?
            .ok_or_else(|| {
                bootstrap_retry_failure(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "regular catalog history lacks its retained migration marker",
                ))
            })?;
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")
                .map_err(bootstrap_retry_failure)?;
        journal = owner
            .committed_migration_journal_for_marker_locked(&marker)
            .map_err(bootstrap_retry_failure)?;
        verify_migration_marker_journal_binding(&marker, &marker_bytes, &journal)
            .map_err(bootstrap_retry_failure)?;
    }
    let journal_disposition = recovered_journal_disposition(Some(&journal))
        .unwrap_or(MigrationMutationDispositionV1::RetryExactPlanRequired);
    if journal.kind != TransactionKindV1::V1Migration {
        return Err(bootstrap_journal_failure(
            ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration registry bootstrap requires a recoverable or committed migration",
            ),
            journal_disposition,
        ));
    }
    if matches!(
        (journal.state, journal.outcome),
        (
            TransactionStateV1::Committed,
            Some(TransactionOutcomeV1::RolledBack)
        )
    ) {
        return Ok(
            MigrationCheckoutRegistryBootstrapV1::RolledBackNotInstalled {
                disposition: journal_disposition,
            },
        );
    }
    return Ok(MigrationCheckoutRegistryBootstrapV1::RequiresRegistry(
        MigrationCheckoutRegistryBootstrapSessionV1 {
            owner,
            journal,
            disposition: journal_disposition,
            lifetime_lock,
            mutation_lock,
        },
    ));
}

impl MigrationCheckoutRegistryBootstrapSessionV1 {
    pub(crate) fn disposition(&self) -> MigrationMutationDispositionV1 {
        self.disposition
    }

    pub(crate) fn bind_registry(
        mut self,
        registry_without_checkouts: MigrationParticipantRegistry,
    ) -> Result<MigrationCheckoutRegistryBoundSessionV1, MigrationStoreOpenFailureV1> {
        let registry_without_checkouts =
            registry_without_checkouts
                .validate()
                .map_err(|error| MigrationStoreOpenFailureV1 {
                    error,
                    disposition: self.disposition,
                })?;
        if registry_without_checkouts.catalog_path != self.owner.paths.catalog
            || !registry_without_checkouts
                .checkout_identity_markers
                .is_empty()
        {
            return Err(MigrationStoreOpenFailureV1 {
                error: ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_registry",
                    "migration bootstrap requires the matching checkout-empty registry",
                ),
                disposition: self.disposition,
            });
        }
        let base_registry = registry_without_checkouts.clone();
        self.owner.registry = ParticipantRegistry::Migration(Arc::new(registry_without_checkouts));
        let auxiliary_locks =
            self.owner
                .acquire_auxiliary_locks()
                .map_err(|error| MigrationStoreOpenFailureV1 {
                    error,
                    disposition: self.disposition,
                })?;
        Ok(MigrationCheckoutRegistryBoundSessionV1 {
            owner: self.owner,
            base_registry,
            journal: self.journal,
            disposition: self.disposition,
            lifetime_lock: self.lifetime_lock,
            mutation_lock: self.mutation_lock,
            auxiliary_locks,
        })
    }
}

impl MigrationCheckoutRegistryBoundSessionV1 {
    pub(crate) fn disposition(&self) -> MigrationMutationDispositionV1 {
        self.disposition
    }

    fn retained_checkout_roots(
        &self,
        discovered_checkout_roots: &BTreeMap<String, PathBuf>,
    ) -> Result<BTreeMap<String, PathBuf>, MigrationBootstrapFailureV1> {
        let journal = &self.journal;
        let owner = &self.owner;
        let journal_disposition = self.disposition;
        let attachment_participant = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Attachments)
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migration registry bootstrap lacks attachment evidence",
                )
            })
            .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
        let ExpectedImageV1::Present {
            sha256: expected_hash,
            artifact_name,
        } = &attachment_participant.new
        else {
            return Err(bootstrap_journal_failure(
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migration registry bootstrap requires an attachment post-image",
                ),
                journal_disposition,
            ));
        };
        let installed = owner
            .io
            .read_regular_nofollow(&owner.paths.attachments, MAX_PROJECT_CATALOG_BYTES)
            .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
        let staged = owner
            .io
            .read_regular_nofollow(
                &owner.paths.stage_dir.join(artifact_name.as_str()),
                MAX_PROJECT_CATALOG_BYTES,
            )
            .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
        let attachment_bytes = installed
            .filter(|bytes| sha256(bytes) == *expected_hash)
            .or_else(|| staged.filter(|bytes| sha256(bytes) == *expected_hash));
        let Some(attachment_bytes) = attachment_bytes else {
            if journal.state == TransactionStateV1::Prepared {
                return checkout_action_roots_for_rollback(
                    journal,
                    discovered_checkout_roots,
                    journal_disposition,
                );
            }
            return Err(bootstrap_journal_failure(
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migration registry bootstrap cannot find the exact attachment post-image",
                ),
                journal_disposition,
            ));
        };
        let attachments = decode_attachment_snapshot(&attachment_bytes)
            .map_err(contract_error)
            .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
        let discovered_by_root = discovered_checkout_roots
            .iter()
            .map(|(observation_id, root)| (root.clone(), observation_id.clone()))
            .collect::<BTreeMap<_, _>>();
        if discovered_by_root.len() != discovered_checkout_roots.len() {
            return Err(bootstrap_journal_failure(
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_registry",
                    "migration registry bootstrap discovered duplicate checkout roots",
                ),
                journal_disposition,
            ));
        }
        let mut retained_checkout_roots = BTreeMap::new();
        let mut roots_by_checkout_id = BTreeMap::new();
        for attachment in attachments.attachments.values() {
            let root = PathBuf::from(&attachment.checkout_dir);
            if !safe_absolute_path(&root) {
                return Err(bootstrap_journal_failure(
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration attachment post-image contains an unsafe checkout root",
                    ),
                    journal_disposition,
                ));
            }
            let observation_id = discovered_by_root
                .get(&root)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration attachment root is absent from strict checkout discovery",
                    )
                })
                .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
            retained_checkout_roots.insert(observation_id.clone(), root.clone());
            if let Some(existing) =
                roots_by_checkout_id.insert(attachment.checkout_id.clone(), root.clone())
                && existing != root
            {
                return Err(bootstrap_journal_failure(
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration attachment checkout id identifies multiple roots",
                    ),
                    journal_disposition,
                ));
            }
        }
        for action in &journal.monotonic_checkout_identity_actions {
            let root = discovered_checkout_roots
                .get(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration action observation is absent from strict checkout discovery",
                    )
                })
                .map_err(|error| bootstrap_journal_failure(error, journal_disposition))?;
            if roots_by_checkout_id.get(&action.planned_id) != Some(root)
                || retained_checkout_roots.get(&action.observation_id) != Some(root)
            {
                return Err(bootstrap_journal_failure(
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration action observation disagrees with the attachment post-image",
                    ),
                    journal_disposition,
                ));
            }
        }
        Ok(retained_checkout_roots)
    }

    pub(crate) fn finish_open(
        mut self,
        discovered_checkout_roots: &BTreeMap<String, PathBuf>,
    ) -> Result<MigrationStoreOpenOutcomeV1, MigrationStoreOpenFailureV1> {
        let retained_checkout_roots = self
            .retained_checkout_roots(discovered_checkout_roots)
            .map_err(|failure| MigrationStoreOpenFailureV1 {
                error: failure.error,
                disposition: failure.disposition,
            })?;
        let mut registry = self.base_registry.clone();
        for (observation_id, root) in retained_checkout_roots {
            registry
                .register_checkout_identity(observation_id, root)
                .map_err(|error| MigrationStoreOpenFailureV1 {
                    error,
                    disposition: self.disposition,
                })?;
        }
        let registry = registry
            .validate()
            .map_err(|error| MigrationStoreOpenFailureV1 {
                error,
                disposition: self.disposition,
            })?;
        self.owner.registry = ParticipantRegistry::Migration(Arc::new(registry));
        self.owner
            .recover_locked()
            .map_err(|error| MigrationStoreOpenFailureV1 {
                error,
                disposition: self.disposition,
            })?;
        let disposition = if self.journal.state == TransactionStateV1::Prepared {
            self.owner
                .read_journal_locked()
                .map_err(|error| MigrationStoreOpenFailureV1 {
                    error,
                    disposition: self.disposition,
                })?
                .as_ref()
                .and_then(|journal| recovered_journal_disposition(Some(journal)))
                .ok_or_else(|| MigrationStoreOpenFailureV1 {
                    error: ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "migration bootstrap recovery did not reach a terminal journal outcome",
                    ),
                    disposition: self.disposition,
                })?
        } else {
            self.disposition
        };
        if disposition == MigrationMutationDispositionV1::RecoveredToOldState {
            drop(self.auxiliary_locks);
            drop(self.mutation_lock);
            return Ok(MigrationStoreOpenOutcomeV1::RolledBackNotInstalled { disposition });
        }
        let current = Arc::new(
            self.owner
                .read_strict_pair_locked()
                .map_err(|error| MigrationStoreOpenFailureV1 { error, disposition })?,
        );
        drop(self.auxiliary_locks);
        drop(self.mutation_lock);
        Ok(MigrationStoreOpenOutcomeV1::Installed(
            MigrationStoreOpenV1 {
                store: ProjectCatalogStore {
                    owner: self.owner,
                    current: RwLock::new(PublishedStoreState::Ready(current)),
                    _lifetime_lock: self.lifetime_lock,
                    commit_observer: CatalogCommitObserver::new(),
                },
                disposition,
            },
        ))
    }
}

fn checkout_action_roots_for_rollback(
    journal: &ProjectCatalogTransactionJournalV1,
    discovered_checkout_roots: &BTreeMap<String, PathBuf>,
    disposition: MigrationMutationDispositionV1,
) -> Result<BTreeMap<String, PathBuf>, MigrationBootstrapFailureV1> {
    journal
        .monotonic_checkout_identity_actions
        .iter()
        .map(|action| {
            discovered_checkout_roots
                .get(&action.observation_id)
                .cloned()
                .map(|root| (action.observation_id.clone(), root))
                .ok_or_else(|| {
                    bootstrap_journal_failure(
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_invalid_migration_registry",
                            "rollback action observation is absent from strict checkout discovery",
                        ),
                        disposition,
                    )
                })
        })
        .collect()
}

fn path_exists_nofollow(path: &Path) -> ProjectCatalogStoreResult<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if exact_enoent(&error) => Ok(false),
        Err(error) => Err(io_error("inspect", path, error)),
    }
}

fn exact_enoent(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    {
        error.raw_os_error() == Some(libc::ENOENT)
    }
    #[cfg(not(unix))]
    {
        error.kind() == std::io::ErrorKind::NotFound
    }
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn safe_absolute_path(path: &Path) -> bool {
    use std::path::Component;

    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(valid_basename)
}

fn published_catalog_scopes(catalog: &CatalogSnapshotV2) -> BTreeSet<PublishedScope> {
    catalog
        .projects
        .values()
        .filter_map(|project| match &project.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal => None,
        })
        .collect()
}

fn migration_inventory_scopes<'a>(
    catalog: &CatalogSnapshotV2,
    participants: impl Iterator<Item = (&'a ParticipantRoleV1, Option<&'a [u8]>)>,
) -> ProjectCatalogStoreResult<BTreeSet<PublishedScope>> {
    let mut scopes = published_catalog_scopes(catalog);
    for (role, post_image) in participants {
        let ParticipantRoleV1::CollisionRetirement { project_id } = role else {
            continue;
        };
        let bytes = post_image.ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                "collision retirement post-image is absent",
            )
        })?;
        let retirement =
            decode_collision_retirement_pending_for_migration(bytes).map_err(|error| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    error.to_string(),
                )
            })?;
        let former_scopes = retirement
            .entries
            .values()
            .map(|entry| entry.former_scope.clone())
            .collect::<BTreeSet<_>>();
        if retirement.project_id != *project_id || former_scopes.len() != 1 {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                "collision retirement role does not define one exact owner scope",
            ));
        }
        scopes.extend(former_scopes);
    }
    Ok(scopes)
}

fn validate_checkout_bindings(
    registry: &MigrationParticipantRegistry,
    attachments: &AttachmentSnapshotV1,
    actions: &[CheckoutIdentityActionV1],
) -> ProjectCatalogStoreResult<()> {
    let fail = |detail: &str| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            format!("checkout identity binding validation failed: {detail}"),
        )
    };
    let mut attachment_roots = BTreeMap::new();
    for attachment in attachments.attachments.values() {
        let root = PathBuf::from(&attachment.checkout_dir);
        if !safe_absolute_path(&root) {
            return Err(fail("attachment checkout root is unsafe"));
        }
        if let Some(existing) = attachment_roots.insert(root, attachment.checkout_id.as_str())
            && existing != attachment.checkout_id.as_str()
        {
            return Err(fail(
                "attachments sharing a checkout root disagree on checkout id",
            ));
        }
    }
    let mut registered_roots = BTreeMap::new();
    for observation_id in registry.checkout_identity_markers.keys() {
        let root = registry
            .checkout_root(observation_id)
            .ok_or_else(|| fail("registered checkout target has no code-owned root"))?;
        if registered_roots
            .insert(root, observation_id.as_str())
            .is_some()
        {
            return Err(fail("checkout root is registered more than once"));
        }
    }
    if attachment_roots.keys().collect::<BTreeSet<_>>()
        != registered_roots.keys().collect::<BTreeSet<_>>()
    {
        return Err(fail(
            "attachment checkout roots and registered roots differ",
        ));
    }
    let actions_by_observation = actions
        .iter()
        .map(|action| (action.observation_id.as_str(), action))
        .collect::<BTreeMap<_, _>>();
    if actions_by_observation.len() != actions.len() {
        return Err(fail("checkout identity action is duplicated"));
    }
    for (root, observation_id) in registered_roots {
        let checkout_id = attachment_roots
            .get(&root)
            .expect("registered and attachment checkout roots were compared");
        if let Some(action) = actions_by_observation.get(observation_id)
            && action.planned_id.as_str() != *checkout_id
        {
            return Err(fail(
                "checkout action id disagrees with the attachment snapshot",
            ));
        }
    }
    if actions_by_observation.keys().any(|observation_id| {
        !registry
            .checkout_identity_markers
            .contains_key(*observation_id)
    }) {
        return Err(fail("checkout action has no registered attachment root"));
    }
    Ok(())
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
pub(crate) struct MigrationParticipantDraftV1 {
    role: ParticipantRoleV1,
    expected_old_sha256: Option<Sha256Hex>,
    post_image: Option<Vec<u8>>,
}

#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
impl MigrationParticipantDraftV1 {
    pub(crate) fn new(
        role: ParticipantRoleV1,
        expected_old_sha256: Option<Sha256Hex>,
        post_image: Option<Vec<u8>>,
    ) -> Self {
        Self {
            role,
            expected_old_sha256,
            post_image,
        }
    }

    pub(crate) fn predicted_post_image(&self) -> Option<(String, Sha256Hex)> {
        self.post_image
            .as_ref()
            .map(|bytes| (self.role.artifact_token(), sha256(bytes)))
    }
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
pub(crate) struct MigrationImmutableAssetDraftV1 {
    role: ImmutableAssetRoleV1,
    source: MigrationImmutableAssetSourceV1,
}

#[derive(Debug, Clone)]
enum MigrationImmutableAssetSourceV1 {
    InstallableBytes(Vec<u8>),
    PinnedExisting { expected_sha256: Sha256Hex },
}

#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
impl MigrationImmutableAssetDraftV1 {
    pub(crate) fn new(role: ImmutableAssetRoleV1, bytes: Vec<u8>) -> Self {
        Self {
            role,
            source: MigrationImmutableAssetSourceV1::InstallableBytes(bytes),
        }
    }

    pub(crate) fn pinned_existing(role: ImmutableAssetRoleV1, expected_sha256: Sha256Hex) -> Self {
        Self {
            role,
            source: MigrationImmutableAssetSourceV1::PinnedExisting { expected_sha256 },
        }
    }

    pub(crate) fn predicted_identity(&self) -> (String, Sha256Hex) {
        let hash = match &self.source {
            MigrationImmutableAssetSourceV1::InstallableBytes(bytes) => sha256(bytes),
            MigrationImmutableAssetSourceV1::PinnedExisting { expected_sha256 } => {
                expected_sha256.clone()
            }
        };
        (self.role.artifact_token(), hash)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MigrationCodeSourceDispositionV1 {
    SurvivingActive,
    SurvivingRetained,
    QuarantinedCollision,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationCodeSourceActivationDraftV1 {
    pub(crate) observation_id: String,
    pub(crate) project_id: ProjectId,
    pub(crate) disposition: MigrationCodeSourceDispositionV1,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationCodeSourceGenerationDraftV1 {
    pub(crate) observation_id: String,
    pub(crate) project_id: ProjectId,
    pub(crate) generation_id: Sha256Hex,
    pub(crate) disposition: MigrationCodeSourceDispositionV1,
}

#[derive(Debug, Clone)]
pub(crate) struct MigrationCodeSourceSnapshotDraftV1 {
    pub(crate) legacy_inventory: MigrationLegacyInventoryV1,
    pub(crate) activations: Vec<MigrationCodeSourceActivationDraftV1>,
    pub(crate) generations: Vec<MigrationCodeSourceGenerationDraftV1>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
pub(crate) struct MigrationCheckoutIdentityActionDraftV1 {
    observation_id: String,
    planned_id: String,
}

#[derive(Debug, Clone)]
pub(crate) enum MigrationPublisherSourceDraftV1 {
    Missing,
    Present(Vec<u8>),
}

#[derive(Debug, Clone)]
pub(crate) enum MigrationLegacyProjectSourceDraftV1 {
    Missing,
    Present(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum MigrationLegacyProjectSourceEvidenceV1 {
    Missing { absence_sha256: Sha256Hex },
    Present { sha256: Sha256Hex },
}

impl MigrationLegacyProjectSourceEvidenceV1 {
    fn missing() -> Self {
        Self::Missing {
            absence_sha256: legacy_project_source_absence_sha256(),
        }
    }

    fn validate(&self) -> ProjectCatalogStoreResult<()> {
        if let Self::Missing { absence_sha256 } = self
            && absence_sha256 != &legacy_project_source_absence_sha256()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                "legacy project source absence fingerprint is invalid",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum MigrationPublisherSourceEvidenceV1 {
    Missing { absence_sha256: Sha256Hex },
    Present { sha256: Sha256Hex },
}

impl MigrationPublisherSourceEvidenceV1 {
    fn missing() -> Self {
        Self::Missing {
            absence_sha256: publisher_source_absence_sha256(),
        }
    }

    fn validate(&self) -> ProjectCatalogStoreResult<()> {
        if let Self::Missing { absence_sha256 } = self
            && absence_sha256 != &publisher_source_absence_sha256()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                "publisher source absence fingerprint is invalid",
            ));
        }
        Ok(())
    }
}

#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
impl MigrationCheckoutIdentityActionDraftV1 {
    pub(crate) fn new(observation_id: String, planned_id: String) -> Self {
        Self {
            observation_id,
            planned_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PublisherPinEvidenceV1 {
    pub(crate) observation_id: String,
    pub(crate) project_id: ProjectId,
    pub(crate) expected_scope: PublishedScope,
    pub(crate) full_ref: FullPublisherRef,
    pub(crate) candidate_attachment_ids: BTreeSet<AttachmentId>,
    pub(crate) resolved_commit: Option<GitObjectId>,
    pub(crate) resolved_scope: Option<PublishedScope>,
    pub(crate) source_observation_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum PublisherDispositionEvidenceV1 {
    SeedG1 {
        observation_id: String,
        project_id: ProjectId,
        attachment_id: AttachmentId,
        expected_scope: PublishedScope,
        full_ref: FullPublisherRef,
        accepted_commit: GitObjectId,
        generation_id: AcceptedPublicationGenerationId,
        generation_sha256: Sha256Hex,
        pointer_sha256: Sha256Hex,
    },
    NoPublishedContentAcknowledged {
        observation_id: String,
        project_id: ProjectId,
        expected_scope: PublishedScope,
        full_ref: FullPublisherRef,
        bounded_reason: String,
    },
}

impl PublisherDispositionEvidenceV1 {
    fn observation_id(&self) -> &str {
        match self {
            Self::SeedG1 { observation_id, .. }
            | Self::NoPublishedContentAcknowledged { observation_id, .. } => observation_id,
        }
    }

    fn project_id(&self) -> &ProjectId {
        match self {
            Self::SeedG1 { project_id, .. }
            | Self::NoPublishedContentAcknowledged { project_id, .. } => project_id,
        }
    }

    fn expected_scope(&self) -> &PublishedScope {
        match self {
            Self::SeedG1 { expected_scope, .. }
            | Self::NoPublishedContentAcknowledged { expected_scope, .. } => expected_scope,
        }
    }

    fn full_ref(&self) -> &FullPublisherRef {
        match self {
            Self::SeedG1 { full_ref, .. }
            | Self::NoPublishedContentAcknowledged { full_ref, .. } => full_ref,
        }
    }
}

fn validate_publisher_evidence(
    pins: &[PublisherPinEvidenceV1],
    dispositions: &[PublisherDispositionEvidenceV1],
    owner: &str,
) -> ProjectCatalogStoreResult<()> {
    if pins.len() > MAX_MIGRATION_PUBLISHER_PINS
        || dispositions.len() > MAX_MIGRATION_PUBLISHER_PINS
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher evidence exceeds its cardinality limit"),
        ));
    }
    let mut pins_by_observation = std::collections::BTreeMap::new();
    let mut scopes = BTreeSet::new();
    let mut candidate_attachment_count = 0_usize;
    let mut source_observation_count = 0_usize;
    let mut encoded_evidence_bytes = 0_usize;
    for pin in pins {
        validate_evidence_id(&pin.observation_id, "publisher pin observation")?;
        pin.expected_scope.validate().map_err(contract_error)?;
        if pin.candidate_attachment_ids.len() > MAX_PROJECT_CATALOG_ENTRIES
            || pin.source_observation_ids.is_empty()
            || pin.source_observation_ids.len() > MAX_PROJECT_CATALOG_ENTRIES
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher pin has invalid source cardinality"),
            ));
        }
        candidate_attachment_count = candidate_attachment_count
            .checked_add(pin.candidate_attachment_ids.len())
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_publisher_evidence",
                    format!("{owner} publisher attachment evidence overflows"),
                )
            })?;
        source_observation_count = source_observation_count
            .checked_add(pin.source_observation_ids.len())
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_publisher_evidence",
                    format!("{owner} publisher source evidence overflows"),
                )
            })?;
        if candidate_attachment_count > MAX_PROJECT_CATALOG_ENTRIES
            || source_observation_count > MAX_PROJECT_CATALOG_ENTRIES
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher evidence exceeds its aggregate cardinality limit"),
            ));
        }
        for observation_id in &pin.source_observation_ids {
            validate_evidence_id(observation_id, "publisher source observation")?;
        }
        if let Some(scope) = &pin.resolved_scope {
            scope.validate().map_err(contract_error)?;
        }
        if pin.resolved_commit.is_some() != pin.resolved_scope.is_some() {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher resolution evidence is incomplete"),
            ));
        }
        if pins_by_observation
            .insert(pin.observation_id.as_str(), pin)
            .is_some()
            || !scopes.insert(pin.expected_scope.clone())
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher pin evidence is duplicated"),
            ));
        }
        add_publisher_evidence_size(&mut encoded_evidence_bytes, pin, owner)?;
    }
    let mut disposition_ids = BTreeSet::new();
    for disposition in dispositions {
        validate_evidence_id(
            disposition.observation_id(),
            "publisher disposition observation",
        )?;
        disposition
            .expected_scope()
            .validate()
            .map_err(contract_error)?;
        let pin = pins_by_observation
            .get(disposition.observation_id())
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_publisher_evidence",
                    format!("{owner} publisher disposition has no matching pin"),
                )
            })?;
        if !disposition_ids.insert(disposition.observation_id())
            || disposition.project_id() != &pin.project_id
            || disposition.expected_scope() != &pin.expected_scope
            || disposition.full_ref() != &pin.full_ref
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher disposition rewrites or duplicates pin evidence"),
            ));
        }
        if let PublisherDispositionEvidenceV1::NoPublishedContentAcknowledged {
            bounded_reason, ..
        } = disposition
            && (bounded_reason.is_empty()
                || bounded_reason.len() > 4096
                || bounded_reason.trim() != bounded_reason
                || bounded_reason.chars().any(char::is_control))
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} no-content acknowledgement reason is invalid"),
            ));
        }
        if let PublisherDispositionEvidenceV1::SeedG1 {
            attachment_id,
            expected_scope,
            accepted_commit,
            ..
        } = disposition
            && (!pin.candidate_attachment_ids.contains(attachment_id)
                || pin.resolved_commit.as_ref() != Some(accepted_commit)
                || pin.resolved_scope.as_ref() != Some(expected_scope))
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher seed does not resolve its exact inventoried row"),
            ));
        }
        add_publisher_evidence_size(&mut encoded_evidence_bytes, disposition, owner)?;
    }
    if disposition_ids != pins_by_observation.keys().copied().collect() {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} does not contain exactly one disposition per publisher pin"),
        ));
    }
    Ok(())
}

fn add_publisher_evidence_size(
    total: &mut usize,
    evidence: &impl Serialize,
    owner: &str,
) -> ProjectCatalogStoreResult<()> {
    let charged = nested_pretty_json_row_charge(evidence).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher evidence could not be encoded: {error}"),
        )
    })?;
    *total = total.checked_add(charged).ok_or_else(|| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher evidence size overflows"),
        )
    })?;
    if *total > MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher evidence exceeds its aggregate byte limit"),
        ));
    }
    Ok(())
}

fn nested_pretty_json_row_charge(evidence: &impl Serialize) -> Result<usize, serde_json::Error> {
    let encoded = serde_json::to_vec_pretty(evidence)?;
    // serde_json's pretty formatter uses two spaces per nesting level. Every
    // charged row sits no more than four additional levels deep in the marker
    // or journal. Eight bytes per emitted newline plus two bytes for the comma
    // and newline between rows therefore overbounds durable nesting overhead.
    let nesting_overhead = encoded
        .iter()
        .filter(|byte| **byte == b'\n')
        .count()
        .saturating_mul(8)
        .saturating_add(2);
    Ok(encoded.len().saturating_add(nesting_overhead))
}

fn add_durable_structural_evidence_size(
    total: &mut usize,
    evidence: &impl Serialize,
    owner: &str,
) -> ProjectCatalogStoreResult<()> {
    let charged = nested_pretty_json_row_charge(evidence).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            format!("{owner} durable structural evidence could not be encoded: {error}"),
        )
    })?;
    *total = total.checked_add(charged).ok_or_else(|| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_durable_evidence_exhausted",
            format!("{owner} aggregate durable-evidence budget is exhausted"),
        )
    })?;
    if *total > MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_durable_evidence_exhausted",
            format!("{owner} aggregate durable-evidence budget is exhausted"),
        ));
    }
    Ok(())
}

fn validate_durable_structural_evidence<P: Serialize, A: Serialize, C: Serialize>(
    participants: &[P],
    immutable_assets: &[A],
    checkout_actions: &[C],
    owner: &str,
) -> ProjectCatalogStoreResult<()> {
    let mut total = 0_usize;
    for participant in participants {
        add_durable_structural_evidence_size(&mut total, participant, owner)?;
    }
    for asset in immutable_assets {
        add_durable_structural_evidence_size(&mut total, asset, owner)?;
    }
    for action in checkout_actions {
        add_durable_structural_evidence_size(&mut total, action, owner)?;
    }
    Ok(())
}

fn validate_publisher_source_binding(
    bytes: &[u8],
    pins: &[PublisherPinEvidenceV1],
    owner: &str,
) -> ProjectCatalogStoreResult<Vec<PublisherRefRow>> {
    let rows = decode_publisher_ref_source_v1(bytes).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher source is invalid: {error}"),
        )
    })?;
    if rows.len() != pins.len() {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher source rows and typed observations differ"),
        ));
    }
    let mut matched = BTreeSet::new();
    for row in &rows {
        let pin = pins
            .iter()
            .find(|pin| {
                pin.expected_scope == row.scope && pin.full_ref.as_str() == row.branch_ref.as_str()
            })
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_publisher_evidence",
                    format!("{owner} publisher source row lacks exact typed evidence"),
                )
            })?;
        if !matched.insert(pin.observation_id.as_str()) {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_publisher_evidence",
                format!("{owner} publisher source row is duplicated"),
            ));
        }
    }
    if matched.len() != pins.len() {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{owner} publisher typed observation lacks an exact source row"),
        ));
    }
    Ok(rows)
}

fn validate_evidence_id(value: &str, kind: &str) -> ProjectCatalogStoreResult<()> {
    if value.is_empty()
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_publisher_evidence",
            format!("{kind} id is invalid"),
        ));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_code_source_snapshot(
    snapshot: &MigrationCodeSourceSnapshotDraftV1,
    resolved_quarantine_bindings: &BTreeSet<(ProjectId, Sha256Hex)>,
    catalog: &CatalogSnapshotV2,
    post_images: &BTreeMap<ParticipantRoleV1, Option<Vec<u8>>>,
    expected_old: &BTreeMap<ParticipantRoleV1, Option<Sha256Hex>>,
    immutable_assets: &[MigrationImmutableAssetEvidenceV1],
    inventory_sha256: &Sha256Hex,
    plan_hash: &Sha256Hex,
) -> ProjectCatalogStoreResult<()> {
    snapshot
        .legacy_inventory
        .validate_evidence()
        .map_err(|error| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                error.to_string(),
            )
        })?;
    fn fail(detail: impl std::fmt::Display) -> ProjectCatalogStoreError {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            format!("code-source snapshot validation failed: {detail}"),
        )
    }
    if snapshot.activations.len() > MAX_PROJECT_CATALOG_ENTRIES
        || snapshot.generations.len() > MAX_MIGRATION_INVENTORY_GENERATIONS
    {
        return Err(fail("source evidence exceeds its cardinality limit"));
    }
    let old_effective = match &snapshot.legacy_inventory.anchor {
        MigrationLegacyAnchorEvidenceV1::Missing => {
            if expected_old
                .get(&ParticipantRoleV1::EffectiveSourceManifest)
                .is_some_and(Option::is_some)
            {
                return Err(fail(
                    "legacy effective source absence disagrees with participant",
                ));
            }
            None
        }
        MigrationLegacyAnchorEvidenceV1::Present {
            bytes,
            sha256: anchor_sha256,
        } => {
            if Sha256Hex::parse(anchor_sha256.clone()).map_err(contract_error)? != sha256(bytes)
                || expected_old
                    .get(&ParticipantRoleV1::EffectiveSourceManifest)
                    .and_then(Option::as_ref)
                    != Some(&sha256(bytes))
            {
                return Err(fail("legacy effective source bytes or hash disagree"));
            }
            Some(
                decode_migration_effective_source_manifest_v1(bytes)
                    .map_err(|error| fail(error.to_string()))?,
            )
        }
    };
    let new_effective_bytes = post_images
        .get(&ParticipantRoleV1::EffectiveSourceManifest)
        .and_then(Option::as_deref)
        .ok_or_else(|| fail("effective source post-image is absent"))?;
    let new_effective: MigrationEffectiveSourceManifestV1 =
        decode_migration_effective_source_manifest_v1(new_effective_bytes)
            .map_err(|error| fail(error.to_string()))?;
    let old_selections = old_effective
        .as_ref()
        .map(|manifest| {
            manifest
                .selections
                .iter()
                .map(|selection| (selection.project_id.clone(), selection))
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let new_selections = new_effective
        .selections
        .iter()
        .map(|selection| (selection.project_id.clone(), selection))
        .collect::<BTreeMap<_, _>>();

    let inventory_activations = snapshot
        .legacy_inventory
        .activations
        .iter()
        .map(|row| (row.project_id.clone(), row))
        .collect::<BTreeMap<_, _>>();
    let inventory_generations = snapshot
        .legacy_inventory
        .generations
        .iter()
        .map(|row| (row.generation_id.as_str(), row))
        .collect::<BTreeMap<_, _>>();
    if inventory_activations.len() != snapshot.legacy_inventory.activations.len()
        || inventory_generations.len() != snapshot.legacy_inventory.generations.len()
    {
        return Err(fail("legacy inventory contains duplicate identities"));
    }

    let mut observation_ids = BTreeSet::new();
    let mut planned_activations = BTreeMap::new();
    for activation in &snapshot.activations {
        validate_evidence_id(
            &activation.observation_id,
            "code-source activation observation",
        )?;
        if !observation_ids.insert(activation.observation_id.as_str())
            || planned_activations
                .insert(activation.project_id.clone(), activation)
                .is_some()
            || !inventory_activations.contains_key(&activation.project_id)
            || matches!(
                activation.disposition,
                MigrationCodeSourceDispositionV1::SurvivingRetained
            )
        {
            return Err(fail(
                "activation disposition is duplicated, unknown, or invalid",
            ));
        }
    }
    if planned_activations.keys().collect::<BTreeSet<_>>()
        != inventory_activations.keys().collect::<BTreeSet<_>>()
    {
        return Err(fail("activation plan omits an inventoried activation"));
    }

    let mut planned_generations = BTreeMap::new();
    for generation in &snapshot.generations {
        validate_evidence_id(
            &generation.observation_id,
            "code-source generation observation",
        )?;
        if !observation_ids.insert(generation.observation_id.as_str())
            || planned_generations
                .insert(generation.generation_id.as_str(), generation)
                .is_some()
            || !inventory_generations.contains_key(generation.generation_id.as_str())
        {
            return Err(fail("generation disposition is duplicated or unknown"));
        }
    }
    if planned_generations.keys().copied().collect::<BTreeSet<_>>()
        != inventory_generations
            .keys()
            .copied()
            .collect::<BTreeSet<_>>()
    {
        return Err(fail("generation plan omits an inventoried generation"));
    }
    let planned_quarantine_bindings = planned_generations
        .values()
        .filter(|generation| {
            generation.disposition == MigrationCodeSourceDispositionV1::QuarantinedCollision
        })
        .map(|generation| {
            (
                generation.project_id.clone(),
                generation.generation_id.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    if &planned_quarantine_bindings != resolved_quarantine_bindings {
        return Err(fail(
            "collision dispositions do not match the resolved quarantine owner bindings",
        ));
    }
    let collision_projects = resolved_quarantine_bindings
        .iter()
        .map(|(project_id, _)| project_id)
        .collect::<BTreeSet<_>>();
    let assigned_collision_project_bindings = planned_generations
        .values()
        .filter(|generation| collision_projects.contains(&generation.project_id))
        .map(|generation| {
            (
                generation.project_id.clone(),
                generation.generation_id.clone(),
            )
        })
        .collect::<BTreeSet<_>>();
    if &assigned_collision_project_bindings != resolved_quarantine_bindings {
        return Err(fail(
            "collision project assignments do not match the resolved quarantine owner bindings",
        ));
    }

    let mut collision_owner_scopes = BTreeMap::new();
    for generation in planned_generations.values().filter(|generation| {
        generation.disposition == MigrationCodeSourceDispositionV1::QuarantinedCollision
    }) {
        let inventory = inventory_generations[generation.generation_id.as_str()];
        match collision_owner_scopes.get(&generation.project_id) {
            Some(scope) if scope != &inventory.published_scope => {
                return Err(fail("collision owner has conflicting published scopes"));
            }
            Some(_) => {}
            None => {
                collision_owner_scopes.insert(
                    generation.project_id.clone(),
                    inventory.published_scope.clone(),
                );
            }
        }
    }
    let mut collision_pending_projects = BTreeSet::new();
    let mut accounted_collision_pending = BTreeSet::new();
    for pending in &snapshot.legacy_inventory.collision_pending {
        if !collision_pending_projects.insert(pending.project_id.clone()) {
            return Err(fail("legacy collision pending owner is duplicated"));
        }
        let retirement_role = ParticipantRoleV1::CollisionRetirement {
            project_id: pending.project_id.clone(),
        };
        if expected_old.get(&retirement_role).and_then(Option::as_ref)
            != Some(&sha256(&pending.bytes))
        {
            return Err(fail(
                "legacy collision pending bytes lack their exact participant pre-image",
            ));
        }
        for (generation_id, entry) in &pending.record.entries {
            let generation = inventory_generations
                .get(generation_id.as_str())
                .ok_or_else(|| fail("legacy collision pending generation is omitted"))?;
            planned_generations
                .get(generation_id.as_str())
                .filter(|planned| {
                    planned.project_id == pending.project_id
                        && planned.disposition
                            == MigrationCodeSourceDispositionV1::QuarantinedCollision
                })
                .ok_or_else(|| fail("legacy collision pending entry is not exactly accounted"))?;
            match &entry.selector_evidence {
                CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) => {
                    if !inventory_activations
                        .get(&pending.project_id)
                        .is_some_and(|activation| {
                            activation.record.generation_id == generation_id.as_str()
                                && activation.record.selector == *selector
                                && activation.record.snapshot_id == entry.snapshot_id
                        })
                    {
                        return Err(fail(
                            "legacy active collision rewrites its activation evidence",
                        ));
                    }
                }
                CollisionRetirementSelectorEvidenceV1::NoDurableSelector => {
                    if inventory_activations
                        .get(&pending.project_id)
                        .is_some_and(|activation| {
                            activation.record.generation_id == generation_id.as_str()
                        })
                        || generation.record.state == bbox_code_source::GenerationState::Active
                    {
                        return Err(fail(
                            "legacy retained collision suppresses active selector authority",
                        ));
                    }
                }
            }
            if generation.published_scope != entry.former_scope {
                return Err(fail(
                    "legacy collision pending scope and generation scope disagree",
                ));
            }
            if collision_owner_scopes.get(&pending.project_id) != Some(&generation.published_scope)
            {
                return Err(fail("collision owner has conflicting published scopes"));
            }
            accounted_collision_pending.insert((pending.project_id.clone(), generation_id.clone()));
        }
    }
    if accounted_collision_pending.len()
        != snapshot
            .legacy_inventory
            .collision_pending
            .iter()
            .map(|row| row.record.entries.len())
            .sum::<usize>()
    {
        return Err(fail(
            "legacy collision pending rows are not consumed exactly once",
        ));
    }
    for generation in planned_generations.values().filter(|generation| {
        generation.disposition == MigrationCodeSourceDispositionV1::QuarantinedCollision
    }) {
        let inventory = inventory_generations[generation.generation_id.as_str()];
        if collision_owner_scopes.get(&generation.project_id) != Some(&inventory.published_scope) {
            return Err(fail(
                "quarantined generation is assigned to the wrong collision owner or scope",
            ));
        }
    }

    let mut accounted_roles = BTreeSet::from([ParticipantRoleV1::EffectiveSourceManifest]);
    let mut accounted_manifests = BTreeSet::new();
    let mut accounted_old_selections = BTreeSet::new();
    let mut accounted_new_selections = BTreeSet::new();

    for evidence in &snapshot.generations {
        let inventory = inventory_generations[evidence.generation_id.as_str()];
        let old_stored = inventory.record.clone();
        let expected_stored = bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
            old_stored,
            inventory.published_scope.clone(),
        )
        .map_err(|error| fail(error.to_string()))?;
        let stored_role = ParticipantRoleV1::StoredGenerationMetadata {
            project_id: evidence.project_id.clone(),
            published_scope: inventory.published_scope.clone(),
            generation_id: evidence.generation_id.clone(),
        };
        if expected_old.get(&stored_role).and_then(Option::as_ref)
            != Some(&sha256(&inventory.metadata_bytes))
        {
            return Err(fail("stored metadata participant omits exact legacy bytes"));
        }
        accounted_roles.insert(stored_role.clone());

        let manifest_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope: inventory.published_scope.clone(),
            generation_id: evidence.generation_id.clone(),
        };
        let pinned_manifest = immutable_assets
            .iter()
            .find(|asset| asset.role == manifest_role)
            .ok_or_else(|| fail("generation lacks its pinned immutable manifest"))?;
        if pinned_manifest.mode != ImmutableAssetModeV1::PinnedExisting
            || pinned_manifest.sha256.as_str() != inventory.manifest_sha256
        {
            return Err(fail("pinned immutable manifest disagrees with inventory"));
        }
        accounted_manifests.insert(manifest_role);

        let project = catalog
            .projects
            .get(&evidence.project_id)
            .ok_or_else(|| fail("code-source project is absent from catalog"))?;
        match evidence.disposition {
            MigrationCodeSourceDispositionV1::SurvivingActive => {
                let activation_plan = planned_activations
                    .get(&evidence.project_id)
                    .filter(|activation| {
                        activation.disposition == MigrationCodeSourceDispositionV1::SurvivingActive
                    })
                    .ok_or_else(|| fail("active generation lacks surviving activation"))?;
                let old_activation = inventory_activations[&evidence.project_id];
                if old_activation.record.generation_id != evidence.generation_id.as_str()
                    || project.scope != ProjectScope::Published(inventory.published_scope.clone())
                {
                    return Err(fail("active generation and activation join disagree"));
                }
                let selection_matches =
                    |selection: &&bbox_code_source_store::MigrationEffectiveSourceSelectionV1| {
                        selection.published_scope == inventory.published_scope
                            && selection.generation_id == evidence.generation_id.as_str()
                            && selection.selector == old_activation.record.selector
                    };
                if old_effective.is_some()
                    && !old_selections
                        .get(&evidence.project_id)
                        .is_some_and(selection_matches)
                    || !new_selections
                        .get(&evidence.project_id)
                        .is_some_and(selection_matches)
                {
                    return Err(fail("active effective selection rewrites source evidence"));
                }
                let _ = activation_plan;
                let activation_bytes = post_images
                    .get(&ParticipantRoleV1::Activation {
                        project_id: evidence.project_id.clone(),
                    })
                    .and_then(Option::as_deref)
                    .ok_or_else(|| fail("surviving activation post-image is absent"))?;
                let activation = decode_activation_v2_for_migration(activation_bytes)
                    .map_err(|error| fail(error.to_string()))?;
                let stored_bytes = post_images
                    .get(&stored_role)
                    .and_then(Option::as_deref)
                    .ok_or_else(|| fail("surviving active generation lacks stored post-image"))?;
                let stored = decode_stored_generation_v2_for_migration(stored_bytes)
                    .map_err(|error| fail(error.to_string()))?;
                let expected_activation =
                    bbox_code_source_store::ActivationRecordV2::from_v1_for_migration(
                        old_activation.record.clone(),
                        &expected_stored,
                    )
                    .map_err(|error| fail(error.to_string()))?;
                activation
                    .validate_against_generation(&stored)
                    .map_err(|error| fail(error.to_string()))?;
                if activation != expected_activation || stored != expected_stored {
                    return Err(fail(
                        "surviving activation and stored post-images rewrite source evidence",
                    ));
                }
                accounted_roles.insert(ParticipantRoleV1::Activation {
                    project_id: evidence.project_id.clone(),
                });
                if old_effective.is_some() {
                    accounted_old_selections.insert(evidence.project_id.clone());
                }
                accounted_new_selections.insert(evidence.project_id.clone());
            }
            MigrationCodeSourceDispositionV1::SurvivingRetained => {
                if inventory_activations.contains_key(&evidence.project_id)
                    && inventory_activations[&evidence.project_id]
                        .record
                        .generation_id
                        == evidence.generation_id.as_str()
                    || project.scope != ProjectScope::Published(inventory.published_scope.clone())
                {
                    return Err(fail("retained generation is active or has the wrong scope"));
                }
                let stored_bytes = post_images
                    .get(&stored_role)
                    .and_then(Option::as_deref)
                    .ok_or_else(|| fail("retained generation lacks stored post-image"))?;
                let stored = decode_stored_generation_v2_for_migration(stored_bytes)
                    .map_err(|error| fail(error.to_string()))?;
                if stored != expected_stored {
                    return Err(fail("retained stored post-image rewrites source evidence"));
                }
            }
            MigrationCodeSourceDispositionV1::QuarantinedCollision => {
                if project.scope != ProjectScope::LegacyLocal {
                    return Err(fail("quarantined generation remains published"));
                }
                let stored_bytes = post_images
                    .get(&stored_role)
                    .and_then(Option::as_deref)
                    .ok_or_else(|| fail("quarantined generation metadata must be retained"))?;
                let stored = decode_stored_generation_v2_for_migration(stored_bytes)
                    .map_err(|error| fail(error.to_string()))?;
                if stored != expected_stored {
                    return Err(fail("quarantined metadata rewrites source evidence"));
                }
                let retirement_role = ParticipantRoleV1::CollisionRetirement {
                    project_id: evidence.project_id.clone(),
                };
                if !collision_pending_projects.contains(&evidence.project_id)
                    && expected_old
                        .get(&retirement_role)
                        .and_then(Option::as_ref)
                        .is_some()
                {
                    return Err(fail(
                        "new collision retirement fabricates a participant pre-image",
                    ));
                }
                let retirement_bytes = post_images
                    .get(&retirement_role)
                    .and_then(Option::as_deref)
                    .ok_or_else(|| fail("quarantined generation lacks retirement record"))?;
                let retirement =
                    decode_collision_retirement_pending_for_migration(retirement_bytes)
                        .map_err(|error| fail(error.to_string()))?;
                let existing_lifecycle = snapshot
                    .legacy_inventory
                    .collision_pending
                    .iter()
                    .find(|pending| pending.project_id == evidence.project_id);
                if let Some(existing) = existing_lifecycle {
                    retirement
                        .validate_transition_from(&existing.record)
                        .map_err(|error| {
                            fail(format!(
                                "existing collision retirement evidence changed: {error}"
                            ))
                        })?;
                    if retirement != existing.record {
                        return Err(fail(
                            "existing collision retirement may not be transitioned by migration",
                        ));
                    }
                }
                let expected_generation_ids = resolved_quarantine_bindings
                    .iter()
                    .filter(|(project_id, _)| project_id == &evidence.project_id)
                    .map(|(_, generation_id)| generation_id.to_string())
                    .collect::<BTreeSet<_>>();
                if retirement.project_id != evidence.project_id
                    || retirement.entries.keys().cloned().collect::<BTreeSet<_>>()
                        != expected_generation_ids
                {
                    return Err(fail(
                        "collision retirement membership does not exactly cover the losing project",
                    ));
                }
                let retirement_entry = retirement
                    .entries
                    .get(evidence.generation_id.as_str())
                    .ok_or_else(|| fail("collision retirement generation entry is absent"))?;
                if (existing_lifecycle.is_none()
                    && (retirement_entry.state != CollisionRetirementLifecycleStateV1::Pending
                        || retirement_entry.inventory_hash != inventory_sha256.as_str()
                        || retirement_entry.plan_hash != plan_hash.as_str()))
                    || retirement_entry.former_scope != inventory.published_scope
                    || retirement_entry.manifest_sha256
                        != inventory.record.descriptor.manifest_sha256
                {
                    return Err(fail("collision retirement rewrites source evidence"));
                }
                if let Some(old_activation) = inventory_activations.get(&evidence.project_id)
                    && old_activation.record.generation_id == evidence.generation_id.as_str()
                {
                    let activation_plan = planned_activations
                        .get(&evidence.project_id)
                        .filter(|activation| {
                            activation.disposition
                                == MigrationCodeSourceDispositionV1::QuarantinedCollision
                        })
                        .ok_or_else(|| fail("collision activation lacks quarantine disposition"))?;
                    let _ = activation_plan;
                    let activation_role = ParticipantRoleV1::Activation {
                        project_id: evidence.project_id.clone(),
                    };
                    if post_images.get(&activation_role) != Some(&None)
                        || new_selections.contains_key(&evidence.project_id)
                    {
                        return Err(fail("quarantined activation remains active or selected"));
                    }
                    if retirement_entry.exact_selector()
                        != Some(old_activation.record.selector.as_str())
                        || retirement_entry.snapshot_id != old_activation.record.snapshot_id
                    {
                        return Err(fail(
                            "active collision retirement rewrites selector evidence",
                        ));
                    }
                    accounted_roles.insert(activation_role);
                    if old_effective.is_some() {
                        accounted_old_selections.insert(evidence.project_id.clone());
                    }
                } else if retirement_entry.selector_evidence
                    != CollisionRetirementSelectorEvidenceV1::NoDurableSelector
                {
                    return Err(fail(
                        "retained-only collision invents materialized selector authority",
                    ));
                }
                accounted_roles.insert(retirement_role);
            }
        }
    }
    if accounted_old_selections != old_selections.keys().cloned().collect::<BTreeSet<_>>()
        || accounted_new_selections != new_selections.keys().cloned().collect::<BTreeSet<_>>()
    {
        return Err(fail(
            "effective source manifest contains an unaccounted selection",
        ));
    }
    for role in post_images.keys() {
        if matches!(
            role,
            ParticipantRoleV1::Activation { .. }
                | ParticipantRoleV1::StoredGenerationMetadata { .. }
                | ParticipantRoleV1::CollisionRetirement { .. }
        ) && !accounted_roles.contains(role)
        {
            return Err(fail(
                "code-source post-image role lacks source snapshot evidence",
            ));
        }
    }
    for asset in immutable_assets {
        if matches!(
            &asset.role,
            ImmutableAssetRoleV1::CollectedGenerationManifest { .. }
        ) && !accounted_manifests.contains(&asset.role)
        {
            return Err(fail(
                "collected manifest pin lacks source snapshot evidence",
            ));
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn validate_new_side_cross_roles(
    catalog: &CatalogSnapshotV2,
    attachments: &AttachmentSnapshotV1,
    post_images: &std::collections::BTreeMap<ParticipantRoleV1, Option<Vec<u8>>>,
    immutable_assets: &[MigrationImmutableAssetEvidenceV1],
    immutable_asset_bytes: &std::collections::BTreeMap<ImmutableAssetRoleV1, Vec<u8>>,
    publisher_pins: &[PublisherPinEvidenceV1],
    publisher_dispositions: &[PublisherDispositionEvidenceV1],
    error_code: &'static str,
) -> ProjectCatalogStoreResult<()> {
    validate_publisher_evidence(
        publisher_pins,
        publisher_dispositions,
        "migration cross-role state",
    )?;
    let fail = |detail: &str| {
        ProjectCatalogStoreError::new(
            error_code,
            format!("migration cross-role validation failed: {detail}"),
        )
    };
    let image_for = |role: &ParticipantRoleV1| {
        post_images
            .get(role)
            .ok_or_else(|| fail("required participant role is missing"))
    };
    let immutable_for = |role: &ImmutableAssetRoleV1| {
        immutable_assets
            .iter()
            .find(|asset| &asset.role == role)
            .ok_or_else(|| fail("required immutable asset role is missing"))
    };
    let effective_bytes = image_for(&ParticipantRoleV1::EffectiveSourceManifest)?
        .as_deref()
        .ok_or_else(|| fail("effective source manifest post-image is absent"))?;
    let effective = decode_migration_effective_source_manifest_v1(effective_bytes)
        .map_err(|error| fail(&error.to_string()))?;
    let effective_by_project = effective
        .selections
        .iter()
        .map(|selection| (selection.project_id.clone(), selection))
        .collect::<BTreeMap<_, _>>();

    let mut collision_owner_scopes = BTreeMap::new();
    for (role, post_image) in post_images {
        let ParticipantRoleV1::CollisionRetirement { project_id } = role else {
            continue;
        };
        let bytes = post_image
            .as_deref()
            .ok_or_else(|| fail("collision retirement post-image is absent"))?;
        let retirement = decode_collision_retirement_pending_for_migration(bytes)
            .map_err(|error| fail(&error.to_string()))?;
        let scopes = retirement
            .entries
            .values()
            .map(|entry| entry.former_scope.clone())
            .collect::<BTreeSet<_>>();
        if retirement.project_id != *project_id || scopes.len() != 1 {
            return Err(fail(
                "collision retirement role does not define one exact owner scope",
            ));
        }
        collision_owner_scopes.insert(
            project_id.clone(),
            scopes
                .into_iter()
                .next()
                .expect("non-empty lifecycle has one scope"),
        );
    }
    let mut stored_generations = std::collections::BTreeMap::new();
    for (role, post_image) in post_images {
        let ParticipantRoleV1::StoredGenerationMetadata {
            project_id,
            published_scope,
            generation_id,
        } = role
        else {
            continue;
        };
        if post_image.is_none() {
            continue;
        }
        let bytes = post_image
            .as_ref()
            .expect("absent stored post-image was handled above");
        let record = decode_stored_generation_v2_for_migration(bytes)
            .map_err(|error| fail(&error.to_string()))?;
        let project = catalog
            .projects
            .get(project_id)
            .ok_or_else(|| fail("stored generation project is absent from the catalog"))?;
        let collision_scope = collision_owner_scopes.get(project_id);
        if record.generation_id != generation_id.as_str()
            || &record.published_scope != published_scope
            || !(project.scope == ProjectScope::Published(published_scope.clone())
                || project.scope == ProjectScope::LegacyLocal
                    && collision_scope == Some(published_scope))
        {
            return Err(fail(
                "stored generation role, record, and catalog scope disagree",
            ));
        }
        let key = (
            project_id.clone(),
            published_scope.clone(),
            generation_id.clone(),
        );
        if stored_generations.insert(key, record).is_some() {
            return Err(fail("stored generation identity is duplicated"));
        }
    }
    let mut activation_projects = BTreeSet::new();
    let mut collision_projects = BTreeSet::new();
    for (role, post_image) in post_images {
        match role {
            ParticipantRoleV1::StoredGenerationMetadata { .. } => {}
            ParticipantRoleV1::Activation { project_id } => {
                if !activation_projects.insert(project_id.clone()) {
                    return Err(fail("activation project is duplicated"));
                }
                let Some(bytes) = post_image else {
                    continue;
                };
                let activation = decode_activation_v2_for_migration(bytes)
                    .map_err(|error| fail(&error.to_string()))?;
                let project = catalog
                    .projects
                    .get(project_id)
                    .ok_or_else(|| fail("activation project is absent from the catalog"))?;
                let generation_id = Sha256Hex::parse(activation.generation_id.clone())
                    .map_err(|_| fail("activation generation id is invalid"))?;
                if &activation.project_id != project_id
                    || project.scope != ProjectScope::Published(activation.published_scope.clone())
                {
                    return Err(fail("activation role, record, and catalog scope disagree"));
                }
                let generation = stored_generations
                    .get(&(
                        project_id.clone(),
                        activation.published_scope.clone(),
                        generation_id,
                    ))
                    .ok_or_else(|| {
                        fail("activation lacks its exact scope-bearing stored generation")
                    })?;
                activation
                    .validate_against_generation(generation)
                    .map_err(|error| fail(&error.to_string()))?;
                let selection = effective_by_project.get(project_id).ok_or_else(|| {
                    fail("active generation lacks its effective source selection")
                })?;
                if selection.published_scope != activation.published_scope
                    || selection.generation_id != activation.generation_id
                    || selection.selector != activation.selector
                {
                    return Err(fail("activation and effective source selection disagree"));
                }
            }
            ParticipantRoleV1::CollisionRetirement { project_id } => {
                if !collision_projects.insert(project_id.clone()) {
                    return Err(fail("collision retirement project is duplicated"));
                }
                let bytes = post_image.as_ref().ok_or_else(|| {
                    fail("collision retirement participant must install a record")
                })?;
                let retirement = decode_collision_retirement_pending_for_migration(bytes)
                    .map_err(|error| fail(&error.to_string()))?;
                let project = catalog
                    .projects
                    .get(project_id)
                    .ok_or_else(|| fail("collision retirement project is absent from catalog"))?;
                let activation_role = ParticipantRoleV1::Activation {
                    project_id: project_id.clone(),
                };
                let exact_entry_count = retirement
                    .entries
                    .values()
                    .filter(|entry| {
                        matches!(
                            entry.selector_evidence,
                            CollisionRetirementSelectorEvidenceV1::ExactMaterialized(_)
                        )
                    })
                    .count();
                let selector_authority_matches = match exact_entry_count {
                    0 => !post_images.contains_key(&activation_role),
                    1 => post_images.get(&activation_role) == Some(&None),
                    _ => false,
                };
                if &retirement.project_id != project_id
                    || project.scope != ProjectScope::LegacyLocal
                    || !selector_authority_matches
                    || effective_by_project.contains_key(project_id)
                {
                    return Err(fail(
                        "collision retirement role, catalog, hashes, or retained source disagree",
                    ));
                }
                for (generation_id, entry) in &retirement.entries {
                    let generation_id = Sha256Hex::parse(generation_id.clone())
                        .map_err(|_| fail("collision retirement generation id is invalid"))?;
                    let manifest_hash = Sha256Hex::parse(entry.manifest_sha256.clone())
                        .map_err(|_| fail("collision retirement manifest hash is invalid"))?;
                    if image_for(&ParticipantRoleV1::StoredGenerationMetadata {
                        project_id: project_id.clone(),
                        published_scope: entry.former_scope.clone(),
                        generation_id: generation_id.clone(),
                    })?
                    .is_none()
                    {
                        return Err(fail(
                            "collision retirement entry rewrites migration evidence",
                        ));
                    }
                    let manifest_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
                        published_scope: entry.former_scope.clone(),
                        generation_id: generation_id.clone(),
                    };
                    let manifest = immutable_for(&manifest_role)?;
                    let stored = stored_generations
                        .get(&(
                            project_id.clone(),
                            entry.former_scope.clone(),
                            generation_id,
                        ))
                        .ok_or_else(|| fail("collision retirement lacks retained metadata"))?;
                    if manifest.mode != ImmutableAssetModeV1::PinnedExisting
                        || stored.descriptor.manifest_sha256 != manifest_hash.as_str()
                    {
                        return Err(fail(
                            "collision retirement lacks exact retained manifest evidence",
                        ));
                    }
                }
            }
            ParticipantRoleV1::Catalog
            | ParticipantRoleV1::Attachments
            | ParticipantRoleV1::EffectiveSourceManifest
            | ParticipantRoleV1::AcceptedPublicationPointer { .. }
            | ParticipantRoleV1::MigrationMarker => {}
        }
    }
    for (role, post_image) in post_images {
        if let ParticipantRoleV1::Activation { project_id } = role
            && post_image.is_none()
            && !collision_projects.contains(project_id)
        {
            return Err(fail(
                "activation removal lacks exact collision retirement evidence",
            ));
        }
    }
    for selection in &effective.selections {
        let activation_role = ParticipantRoleV1::Activation {
            project_id: selection.project_id.clone(),
        };
        if !post_images
            .get(&activation_role)
            .is_some_and(Option::is_some)
        {
            return Err(fail(
                "effective source selection lacks its active generation",
            ));
        }
    }
    for asset in immutable_assets {
        if let ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope,
            generation_id,
        } = &asset.role
        {
            let accounted = post_images.iter().any(|(role, image)| {
                let ParticipantRoleV1::CollisionRetirement { project_id } = role else {
                    return false;
                };
                let Some(bytes) = image else {
                    return false;
                };
                let Ok(retirement) = decode_collision_retirement_pending_for_migration(bytes)
                else {
                    return false;
                };
                retirement.project_id == *project_id
                    && retirement
                        .entries
                        .get(generation_id.as_str())
                        .is_some_and(|entry| {
                            &entry.former_scope == published_scope
                                && entry.manifest_sha256 == asset.sha256.as_str()
                        })
            }) || post_images.iter().any(|(role, image)| {
                matches!(
                    role,
                    ParticipantRoleV1::StoredGenerationMetadata {
                        published_scope: role_scope,
                        generation_id: role_generation,
                        ..
                    } if role_scope == published_scope && role_generation == generation_id
                ) && image.is_some()
            });
            if !accounted {
                return Err(fail(
                    "pinned collected manifest has no stored or collision evidence",
                ));
            }
        }
    }

    let limits = AcceptedPublicationLimits::default();
    for pin in publisher_pins {
        let project = catalog
            .projects
            .get(&pin.project_id)
            .ok_or_else(|| fail("publisher pin project is absent from catalog"))?;
        if project.scope != ProjectScope::Published(pin.expected_scope.clone()) {
            return Err(fail("publisher pin scope disagrees with catalog"));
        }
    }
    let mut seed_projects = BTreeSet::new();
    let mut seed_generation_roles = BTreeSet::new();
    let mut seed_pointer_roles = BTreeSet::new();
    for disposition in publisher_dispositions {
        let PublisherDispositionEvidenceV1::SeedG1 {
            project_id,
            attachment_id,
            expected_scope,
            full_ref,
            accepted_commit,
            generation_id,
            generation_sha256,
            pointer_sha256,
            ..
        } = disposition
        else {
            continue;
        };
        if !seed_projects.insert(project_id.clone()) {
            return Err(fail("publisher seed project is duplicated"));
        }
        let project = catalog
            .projects
            .get(project_id)
            .ok_or_else(|| fail("publisher seed project is absent from catalog"))?;
        let attachment = attachments
            .attachments
            .get(attachment_id)
            .ok_or_else(|| fail("publisher seed attachment is absent"))?;
        if project.scope != ProjectScope::Published(expected_scope.clone())
            || &attachment.project_id != project_id
            || attachment.validated_scope.as_ref() != Some(expected_scope)
            || attachment.status != AttachmentStatus::Attached
        {
            return Err(fail(
                "publisher seed catalog project and attachment do not prove its scope",
            ));
        }
        let pointer_role = ParticipantRoleV1::AcceptedPublicationPointer {
            project_id: project_id.clone(),
        };
        let pointer_bytes = image_for(&pointer_role)?
            .as_deref()
            .ok_or_else(|| fail("publisher seed pointer post-image is absent"))?;
        let pointer =
            decode_pointer_v1(pointer_bytes, &limits).map_err(|error| fail(&error.to_string()))?;
        let generation_role = ImmutableAssetRoleV1::AcceptedPublicationGeneration {
            project_id: project_id.clone(),
            generation_id: generation_id.clone(),
        };
        let generation_asset = immutable_for(&generation_role)?;
        let generation_bytes = immutable_asset_bytes
            .get(&generation_role)
            .ok_or_else(|| fail("publisher seed generation bytes are absent"))?;
        let generation = decode_generation_v1(generation_bytes, &limits)
            .map_err(|error| fail(&error.to_string()))?;
        verify_pointer_generation_v1(&pointer, generation_bytes, &limits)
            .map_err(|error| fail(&error.to_string()))?;
        if &pointer.project_id != project_id
            || &pointer.attachment_id != attachment_id
            || &pointer.full_ref != full_ref
            || &pointer.accepted_commit != accepted_commit
            || &pointer.accepted_scope != expected_scope
            || &pointer.accepted_generation != generation_id
            || pointer.generation_hash.as_str() != generation_sha256.as_str()
            || sha256(pointer_bytes) != *pointer_sha256
            || generation_asset.mode != ImmutableAssetModeV1::Installable
            || generation_asset.sha256 != *generation_sha256
            || sha256(generation_bytes) != *generation_sha256
            || &generation.project_id != project_id
            || &generation.scope != expected_scope
            || &generation.full_ref != full_ref
            || &generation.accepted_commit != accepted_commit
        {
            return Err(fail(
                "publisher seed evidence, pointer, generation, and hashes disagree",
            ));
        }
        seed_pointer_roles.insert(pointer_role);
        seed_generation_roles.insert(generation_role);
    }
    for (role, post_image) in post_images {
        if let ParticipantRoleV1::AcceptedPublicationPointer { .. } = role
            && (post_image.is_none() || !seed_pointer_roles.contains(role))
        {
            return Err(fail(
                "accepted publication pointer is not accounted by one publisher seed",
            ));
        }
    }
    for asset in immutable_assets {
        if matches!(
            &asset.role,
            ImmutableAssetRoleV1::AcceptedPublicationGeneration { .. }
        ) && !seed_generation_roles.contains(&asset.role)
        {
            return Err(fail(
                "accepted publication generation is not accounted by one publisher seed",
            ));
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B transaction input consumed by P1-C.
pub(crate) struct MigrationPlanDraftV1 {
    pub(crate) transaction_id: ProjectCatalogTransactionId,
    pub(crate) plan_hash: Sha256Hex,
    pub(crate) report_artifact_sha256: Sha256Hex,
    pub(crate) resolution_artifact_sha256: Sha256Hex,
    pub(crate) legacy_project_source: MigrationLegacyProjectSourceDraftV1,
    pub(crate) publisher_ref_source: MigrationPublisherSourceDraftV1,
    pub(crate) inventory_sha256: Sha256Hex,
    pub(crate) code_source_inventory_sha256: Sha256Hex,
    pub(crate) catalog: CatalogSnapshotV2,
    pub(crate) attachments: AttachmentSnapshotV1,
    pub(crate) participants: Vec<MigrationParticipantDraftV1>,
    pub(crate) immutable_assets: Vec<MigrationImmutableAssetDraftV1>,
    pub(crate) code_source_snapshot: MigrationCodeSourceSnapshotDraftV1,
    pub(crate) quarantine_authority: ValidatedQuarantineBindingsV1,
    pub(crate) publisher_pins: Vec<PublisherPinEvidenceV1>,
    pub(crate) publisher_dispositions: Vec<PublisherDispositionEvidenceV1>,
    pub(crate) checkout_identity_actions: Vec<MigrationCheckoutIdentityActionDraftV1>,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // P1-B validated plan consumed by P1-C.
pub(crate) struct ValidatedMigrationPlanV1 {
    registry: MigrationParticipantRegistry,
    journal: ProjectCatalogTransactionJournalV1,
    post_images: std::collections::BTreeMap<ParticipantRoleV1, Option<Vec<u8>>>,
    immutable_asset_bytes: std::collections::BTreeMap<ImmutableAssetRoleV1, Vec<u8>>,
    code_source_snapshot: MigrationCodeSourceSnapshotDraftV1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationParticipantArtifactIdentityV1 {
    pub(crate) role: ParticipantRoleV1,
    pub(crate) old_sha256: Option<Sha256Hex>,
    pub(crate) new_sha256: Option<Sha256Hex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationImmutableAssetIdentityV1 {
    pub(crate) role: ImmutableAssetRoleV1,
    pub(crate) sha256: Sha256Hex,
}

/// Path-redacted durable identity returned to the migration facade.
///
/// Private marker and journal wire types never cross this seam. The facade
/// receives only exact reviewed-artifact identity and code-owned role
/// evidence suitable for verification receipts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MigrationArtifactIdentityV1 {
    pub(crate) transaction_id: ProjectCatalogTransactionId,
    pub(crate) plan_hash: Sha256Hex,
    pub(crate) inventory_sha256: Sha256Hex,
    pub(crate) report_artifact_sha256: Sha256Hex,
    pub(crate) resolution_artifact_sha256: Sha256Hex,
    pub(crate) observed_marker_sha256: Sha256Hex,
    pub(crate) participants: Vec<MigrationParticipantArtifactIdentityV1>,
    pub(crate) immutable_assets: Vec<MigrationImmutableAssetIdentityV1>,
    pub(crate) migration_install_is_current: bool,
    pub(crate) epoch: u64,
    pub(crate) checkout_action_count: u64,
    pub(crate) publisher_pin_count: u64,
    pub(crate) quarantine_root_count: u64,
}

impl ValidatedMigrationPlanV1 {
    #[allow(dead_code)] // P1-C consumes the exact pre-install receipt projection.
    pub(crate) fn artifact_identity(&self) -> MigrationArtifactIdentityV1 {
        let marker_bytes = self
            .post_images
            .get(&ParticipantRoleV1::MigrationMarker)
            .and_then(Option::as_deref)
            .expect("validated migration plan has marker post-image bytes");
        migration_artifact_identity_from_journal(
            &self.journal,
            self.marker()
                .expect("validated migration plan has a valid marker"),
            sha256(marker_bytes),
            true,
        )
        .expect("validated migration plan journal and marker agree")
    }

    fn marker(&self) -> ProjectCatalogStoreResult<ProjectCatalogMigrationMarkerV1> {
        let bytes = self
            .post_images
            .get(&ParticipantRoleV1::MigrationMarker)
            .and_then(Option::as_deref)
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "validated migration plan lacks its marker post-image",
                )
            })?;
        decode_bounded_json(bytes, MAX_MARKER_BYTES, "migration marker")
    }
}

#[allow(dead_code)] // P1-B validation seam consumed by P1-C.
pub(crate) fn validate_migration_plan(
    paths: &Path,
    registry: MigrationParticipantRegistry,
    draft: MigrationPlanDraftV1,
) -> ProjectCatalogStoreResult<ValidatedMigrationPlanV1> {
    let registry = registry.validate()?;
    let store_paths = ProjectCatalogPaths::derive(paths)?;
    let authority_plan_hash = Sha256Hex::parse(draft.quarantine_authority.plan_hash().to_string())
        .map_err(contract_error)?;
    if authority_plan_hash != draft.plan_hash
        || draft.quarantine_authority.transaction_id() != &draft.transaction_id
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "quarantine authority is bound to a different canonical plan identity",
        ));
    }
    let authority_generation_owners = draft
        .quarantine_authority
        .generation_owners()
        .iter()
        .map(|(generation_id, project_id)| {
            Ok((
                Sha256Hex::parse(generation_id.to_string()).map_err(contract_error)?,
                project_id.clone(),
            ))
        })
        .collect::<ProjectCatalogStoreResult<BTreeMap<_, _>>>()?;
    let draft_generation_owners = draft
        .code_source_snapshot
        .generations
        .iter()
        .map(|generation| {
            Ok((
                generation.generation_id.clone(),
                generation.project_id.clone(),
            ))
        })
        .collect::<ProjectCatalogStoreResult<BTreeMap<_, _>>>()?;
    if authority_generation_owners != draft_generation_owners {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "quarantine authority is bound to different canonical generation owners",
        ));
    }
    let resolved_quarantine_bindings = draft
        .quarantine_authority
        .bindings()
        .iter()
        .map(|(project_id, generation_id)| {
            Ok((
                project_id.clone(),
                Sha256Hex::parse(generation_id.to_string()).map_err(contract_error)?,
            ))
        })
        .collect::<ProjectCatalogStoreResult<BTreeSet<_>>>()?;
    if draft.participants.len().saturating_add(3) > MAX_MIGRATION_PARTICIPANTS
        || draft.immutable_assets.len() > MAX_MIGRATION_IMMUTABLE_ASSETS
        || draft.checkout_identity_actions.len() > MAX_MIGRATION_CHECKOUT_ACTIONS
        || draft.publisher_pins.len() > MAX_MIGRATION_PUBLISHER_PINS
        || draft.publisher_dispositions.len() > MAX_MIGRATION_PUBLISHER_PINS
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration plan exceeds its cardinality limit",
        ));
    }
    if registry.catalog_path != store_paths.catalog {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_registry",
            "migration registry is bound to a different project catalog path",
        ));
    }
    validate_publisher_evidence(
        &draft.publisher_pins,
        &draft.publisher_dispositions,
        "migration plan",
    )?;
    let publisher_source_evidence = match &draft.publisher_ref_source {
        MigrationPublisherSourceDraftV1::Missing => {
            if !draft.publisher_pins.is_empty() || !draft.publisher_dispositions.is_empty() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_publisher_evidence",
                    "missing publisher source cannot have typed publisher rows",
                ));
            }
            MigrationPublisherSourceEvidenceV1::missing()
        }
        MigrationPublisherSourceDraftV1::Present(bytes) => {
            validate_publisher_source_binding(bytes, &draft.publisher_pins, "migration plan")?;
            MigrationPublisherSourceEvidenceV1::Present {
                sha256: sha256(bytes),
            }
        }
    };
    let (legacy_project_source_evidence, expected_legacy_catalog_sha256) =
        match &draft.legacy_project_source {
            MigrationLegacyProjectSourceDraftV1::Missing => {
                (MigrationLegacyProjectSourceEvidenceV1::missing(), None)
            }
            MigrationLegacyProjectSourceDraftV1::Present(bytes) => (
                MigrationLegacyProjectSourceEvidenceV1::Present {
                    sha256: sha256(bytes),
                },
                Some(sha256(bytes)),
            ),
        };
    validate_catalog_attachments(&draft.catalog, &draft.attachments).map_err(contract_error)?;
    if draft.catalog.epoch != 1
        || draft.attachments.epoch != 1
        || draft.catalog.origin
            != (CatalogOriginV2::MigratedV1 {
                transaction_id: draft.transaction_id.clone(),
            })
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration post-images must start at epoch one with the exact transaction origin",
        ));
    }
    let catalog_bytes = encode_catalog_snapshot(&draft.catalog).map_err(contract_error)?;
    let attachment_bytes =
        encode_attachment_snapshot(&draft.attachments).map_err(contract_error)?;
    let mut post_images = std::collections::BTreeMap::from([
        (ParticipantRoleV1::Catalog, Some(catalog_bytes)),
        (ParticipantRoleV1::Attachments, Some(attachment_bytes)),
    ]);
    let mut expected_old = std::collections::BTreeMap::from([
        (
            ParticipantRoleV1::Catalog,
            expected_legacy_catalog_sha256.clone(),
        ),
        (ParticipantRoleV1::Attachments, None),
    ]);
    for participant in draft.participants {
        if matches!(
            participant.role,
            ParticipantRoleV1::Catalog
                | ParticipantRoleV1::Attachments
                | ParticipantRoleV1::MigrationMarker
        ) || post_images
            .insert(participant.role.clone(), participant.post_image)
            .is_some()
            || expected_old
                .insert(participant.role, participant.expected_old_sha256)
                .is_some()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                "migration plan has a duplicate or owner-controlled participant role",
            ));
        }
    }
    if !post_images.contains_key(&ParticipantRoleV1::EffectiveSourceManifest) {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration plan lacks the complete effective source manifest",
        ));
    }
    if post_images.iter().any(|(role, image)| {
        image
            .as_ref()
            .is_some_and(|bytes| bytes.len() > role.max_bytes())
    }) {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration mutable post-image exceeds its byte limit",
        ));
    }
    for role in post_images.keys() {
        if !matches!(
            role,
            ParticipantRoleV1::Catalog | ParticipantRoleV1::Attachments
        ) && registry.participant_target(role).is_none()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "migration plan role has no code-owned target",
            ));
        }
    }

    let mut immutable_assets_by_role = std::collections::BTreeMap::new();
    let mut immutable_asset_bytes = std::collections::BTreeMap::new();
    for asset in draft.immutable_assets {
        let role = asset.role;
        let (mode, hash, bytes) = match asset.source {
            MigrationImmutableAssetSourceV1::InstallableBytes(bytes) => {
                if role.required_mode() != ImmutableAssetModeV1::Installable
                    || bytes.len() > role.max_bytes()
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "migration installable immutable asset has an invalid role or byte size",
                    ));
                }
                let hash = sha256(&bytes);
                (ImmutableAssetModeV1::Installable, hash, Some(bytes))
            }
            MigrationImmutableAssetSourceV1::PinnedExisting { expected_sha256 } => {
                if role.required_mode() != ImmutableAssetModeV1::PinnedExisting {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "migration pinned immutable asset has an invalid role",
                    ));
                }
                (ImmutableAssetModeV1::PinnedExisting, expected_sha256, None)
            }
        };
        if immutable_assets_by_role
            .insert(role.clone(), (mode, hash.clone()))
            .is_some()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_plan",
                "migration immutable asset role is duplicated",
            ));
        }
        if let Some(bytes) = bytes {
            immutable_asset_bytes.insert(role, bytes);
        }
    }
    match (
        &draft.legacy_project_source,
        &legacy_project_source_evidence,
    ) {
        (
            MigrationLegacyProjectSourceDraftV1::Missing,
            MigrationLegacyProjectSourceEvidenceV1::Missing { .. },
        ) => {
            if immutable_assets_by_role
                .contains_key(&ImmutableAssetRoleV1::LegacyProjectStoreBackup)
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "missing legacy project source must not fabricate a backup",
                ));
            }
        }
        (
            MigrationLegacyProjectSourceDraftV1::Present(expected_bytes),
            MigrationLegacyProjectSourceEvidenceV1::Present { sha256: expected },
        ) => {
            let asset_hash = immutable_assets_by_role
                .get(&ImmutableAssetRoleV1::LegacyProjectStoreBackup)
                .map(|(_, hash)| hash);
            let backup_bytes =
                immutable_asset_bytes.get(&ImmutableAssetRoleV1::LegacyProjectStoreBackup);
            if asset_hash != Some(expected)
                || backup_bytes != Some(expected_bytes)
                || sha256(expected_bytes) != *expected
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "legacy project source backup does not match exact present source bytes",
                ));
            }
        }
        _ => unreachable!("legacy project source draft and evidence are built together"),
    }
    match (&draft.publisher_ref_source, &publisher_source_evidence) {
        (
            MigrationPublisherSourceDraftV1::Missing,
            MigrationPublisherSourceEvidenceV1::Missing { .. },
        ) => {
            if immutable_assets_by_role
                .contains_key(&ImmutableAssetRoleV1::LegacyPublisherRefBackup)
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "missing publisher source must not fabricate a backup",
                ));
            }
        }
        (
            MigrationPublisherSourceDraftV1::Present(expected_bytes),
            MigrationPublisherSourceEvidenceV1::Present { sha256: expected },
        ) => {
            let asset_hash = immutable_assets_by_role
                .get(&ImmutableAssetRoleV1::LegacyPublisherRefBackup)
                .map(|(_, hash)| hash);
            let backup_bytes =
                immutable_asset_bytes.get(&ImmutableAssetRoleV1::LegacyPublisherRefBackup);
            if asset_hash != Some(expected)
                || backup_bytes != Some(expected_bytes)
                || sha256(expected_bytes) != *expected
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "publisher source backup does not match exact present source bytes",
                ));
            }
        }
        _ => unreachable!("publisher source draft and evidence are built together"),
    }

    let actions = draft
        .checkout_identity_actions
        .into_iter()
        .map(|action| CheckoutIdentityActionV1 {
            observation_id: action.observation_id,
            planned_id: action.planned_id,
        })
        .collect::<Vec<_>>();
    let action_ids = actions
        .iter()
        .map(|action| action.observation_id.as_str())
        .collect::<BTreeSet<_>>();
    if action_ids.len() != actions.len()
        || actions.iter().any(|action| {
            registry
                .checkout_identity_target(&action.observation_id)
                .is_none()
        })
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration checkout identity actions are duplicated or unregistered",
        ));
    }
    validate_checkout_bindings(&registry, &draft.attachments, &actions)?;

    let marker_participants = post_images
        .iter()
        .map(|(role, post_image)| {
            build_transaction_participant(
                &draft.transaction_id,
                role.clone(),
                expected_old.get(role).cloned().flatten(),
                post_image,
            )
        })
        .collect::<ProjectCatalogStoreResult<Vec<_>>>()?;
    let participant_evidence = marker_participants
        .iter()
        .map(|participant| MigrationParticipantEvidenceV1 {
            role: participant.role.clone(),
            old: participant.old.clone(),
            new: participant.new.clone(),
        })
        .collect::<Vec<_>>();
    let immutable_evidence = immutable_assets_by_role
        .iter()
        .map(|(role, (mode, hash))| {
            let validated_name = immutable_target_name(&draft.transaction_id, role, &hash)?;
            Ok(MigrationImmutableAssetEvidenceV1 {
                role: role.clone(),
                mode: *mode,
                sha256: hash.clone(),
                validated_name,
            })
        })
        .collect::<ProjectCatalogStoreResult<Vec<_>>>()?;
    if Sha256Hex::parse(
        draft
            .code_source_snapshot
            .legacy_inventory
            .canonical_sha256
            .clone(),
    )
    .map_err(contract_error)?
        != draft.code_source_inventory_sha256
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_migration_plan",
            "migration inventory hash does not bind the owner-enumerated source row set",
        ));
    }
    validate_code_source_snapshot(
        &draft.code_source_snapshot,
        &resolved_quarantine_bindings,
        &draft.catalog,
        &post_images,
        &expected_old,
        &immutable_evidence,
        &draft.inventory_sha256,
        &draft.plan_hash,
    )?;
    validate_new_side_cross_roles(
        &draft.catalog,
        &draft.attachments,
        &post_images,
        &immutable_evidence,
        &immutable_asset_bytes,
        &draft.publisher_pins,
        &draft.publisher_dispositions,
        "error.project_catalog_invalid_migration_plan",
    )?;
    let marker = ProjectCatalogMigrationMarkerV1 {
        version: MIGRATION_MARKER_VERSION,
        transaction_id: draft.transaction_id.clone(),
        plan_hash: draft.plan_hash.clone(),
        report_artifact_sha256: draft.report_artifact_sha256.clone(),
        resolution_artifact_sha256: draft.resolution_artifact_sha256.clone(),
        legacy_project_source: legacy_project_source_evidence.clone(),
        publisher_ref_source: publisher_source_evidence.clone(),
        inventory_sha256: draft.inventory_sha256.clone(),
        publisher_pins: draft.publisher_pins.clone(),
        publisher_dispositions: draft.publisher_dispositions.clone(),
        participants: participant_evidence,
        immutable_assets: immutable_evidence,
        migration_epoch: 1,
    };
    marker.validate()?;
    let marker_bytes = encode_bounded_json(&marker, MAX_MARKER_BYTES, "migration marker")?;
    post_images.insert(ParticipantRoleV1::MigrationMarker, Some(marker_bytes));
    expected_old.insert(ParticipantRoleV1::MigrationMarker, None);

    let participants = post_images
        .iter()
        .map(|(role, post_image)| {
            build_transaction_participant(
                &draft.transaction_id,
                role.clone(),
                expected_old.get(role).cloned().flatten(),
                post_image,
            )
        })
        .collect::<ProjectCatalogStoreResult<Vec<_>>>()?;
    let immutable_assets = immutable_assets_by_role
        .iter()
        .map(|(role, (mode, hash))| {
            let validated_name = immutable_target_name(&draft.transaction_id, role, &hash)?;
            Ok(ImmutableAssetV1 {
                role: role.clone(),
                mode: *mode,
                sha256: hash.clone(),
                validated_name,
                stage_name: match mode {
                    ImmutableAssetModeV1::Installable => {
                        Some(immutable_stage_name(&draft.transaction_id, role, &hash)?)
                    }
                    ImmutableAssetModeV1::PinnedExisting => None,
                },
            })
        })
        .collect::<ProjectCatalogStoreResult<Vec<_>>>()?;
    let journal = ProjectCatalogTransactionJournalV1 {
        version: JOURNAL_VERSION,
        transaction_id: draft.transaction_id,
        kind: TransactionKindV1::V1Migration,
        state: TransactionStateV1::Prepared,
        outcome: None,
        plan_hash: Some(draft.plan_hash),
        report_artifact_sha256: Some(draft.report_artifact_sha256),
        resolution_artifact_sha256: Some(draft.resolution_artifact_sha256),
        legacy_project_source: Some(legacy_project_source_evidence),
        publisher_ref_source: Some(publisher_source_evidence),
        publisher_pins: draft.publisher_pins,
        publisher_dispositions: draft.publisher_dispositions,
        resolved_quarantine_bindings: Some(resolved_quarantine_bindings),
        old_epoch: 0,
        new_epoch: 1,
        participants,
        immutable_assets,
        monotonic_checkout_identity_actions: actions,
        created_at: unix_timestamp()?,
        committed_at: None,
    };
    journal.validate()?;
    let _ = encode_bounded_json(&journal, MAX_JOURNAL_BYTES, "transaction journal")?;

    let mut derived_targets = BTreeSet::from([
        store_paths.catalog.clone(),
        store_paths.attachments.clone(),
        store_paths.migration_marker.clone(),
        store_paths.migration_receipt.clone(),
        store_paths.migration_assets_dir.clone(),
        store_paths.stage_dir.clone(),
        store_paths.backup_dir.clone(),
        store_paths.mutation_lock.clone(),
        store_paths.lifetime_lock.clone(),
        registry.legacy_publisher_ref_source.clone(),
    ]);
    for auxiliary in registry.auxiliary_store_paths() {
        derived_targets.insert(canonical_store_lock_path(&auxiliary));
    }
    for role in post_images.keys() {
        let target = match role {
            ParticipantRoleV1::Catalog => store_paths.catalog.clone(),
            ParticipantRoleV1::Attachments => store_paths.attachments.clone(),
            ParticipantRoleV1::MigrationMarker => store_paths.migration_marker.clone(),
            role => registry.participant_target(role).ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_registry",
                    "migration participant lacks a code-derived target",
                )
            })?,
        };
        if !derived_targets.insert(target)
            && !matches!(
                role,
                ParticipantRoleV1::Catalog
                    | ParticipantRoleV1::Attachments
                    | ParticipantRoleV1::MigrationMarker
            )
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "migration participant targets collide",
            ));
        }
    }
    for asset in &journal.immutable_assets {
        if !derived_targets.insert(registry.immutable_target(&asset.role, &asset.validated_name)) {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "migration immutable target collides with a mutable target",
            ));
        }
    }
    for action in &journal.monotonic_checkout_identity_actions {
        let target = registry
            .checkout_identity_target(&action.observation_id)
            .expect("validated checkout identity target");
        if !derived_targets.insert(target) {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_migration_registry",
                "checkout identity target collides with another migration target",
            ));
        }
    }
    Ok(ValidatedMigrationPlanV1 {
        registry,
        journal,
        post_images,
        immutable_asset_bytes,
        code_source_snapshot: draft.code_source_snapshot,
    })
}

/// Capture a read-only migration preflight while excluding every compatible
/// writer to the legacy project store.
///
/// The lock order is the shared process-lifetime migration lock followed by
/// the canonical project-store mutation lock. P1-C owns the inventory shape,
/// but must perform its complete live v1 capture through this seam.
#[allow(dead_code)] // P1-B seam consumed by the P1-C preflight implementation.
pub(crate) fn capture_migration_preflight<T>(
    projects_path: &Path,
    capture: impl FnOnce() -> ProjectCatalogStoreResult<T>,
) -> ProjectCatalogStoreResult<T> {
    capture_migration_preflight_with(projects_path, |error| error, capture)
}

/// Run a read-only preflight capture under the shared lifetime lock AND the
/// store mutation lock.
///
/// PRECONDITION, and it is a deadlock if you break it: the `capture` closure
/// MUST NOT open a [`ProjectCatalogStore`]. This function holds the store
/// mutation lock across the closure, and `open_existing` acquires that same
/// mutation lock itself through a SECOND file descriptor
/// (`open_existing_with_registry_and_io`), so the open blocks forever on this
/// process's own exclusive flock. It is a hang with no diagnostic, not an
/// error.
///
/// This hazard class is already named in the codebase for the LIFETIME lock:
/// `open_admin_store` downgrades its exclusive guard to shared before opening
/// precisely because, in its own words, "holding exclusive across the open
/// would deadlock against it" (plan section 4.2). The mutation lock has the
/// same property and no downgrade to soften it.
///
/// So this helper is for captures that read RAW FILES with no store open - the
/// v1 migration captures, where there is no v2 store to provide inner locking
/// and this outer mutation lock is the only pair-read coherence there is
/// (plan section 4.1, scoped to raw-file captures). A caller that wants a
/// v2 store should open it directly and let the open take both locks itself,
/// as the backfill and rebuild preflights do.
pub(crate) fn capture_migration_preflight_with<T, E>(
    projects_path: &Path,
    map_lock_error: impl Fn(ProjectCatalogStoreError) -> E,
    capture: impl FnOnce() -> Result<T, E>,
) -> Result<T, E> {
    let paths = ProjectCatalogPaths::derive(projects_path).map_err(&map_lock_error)?;
    let _lifetime_lock = ProjectCatalogMigrationLock::acquire_shared(&paths.catalog)
        .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))
        .map_err(&map_lock_error)?;
    let _mutation_lock = acquire_store_lock_nofollow(&paths.catalog)
        .map_err(|error| io_error("acquire mutation lock for", &paths.catalog, error))
        .map_err(map_lock_error)?;
    capture()
}

#[allow(dead_code)] // P1-B apply seam consumed by P1-C.
pub(crate) fn transact_migration(
    projects_path: &Path,
    plan: ValidatedMigrationPlanV1,
) -> ProjectCatalogStoreResult<ProjectCatalogCommit> {
    transact_migration_classified_with_io(projects_path, plan, Arc::new(RealCatalogStoreIo))
        .map_err(|failure| failure.error)
}

pub(crate) fn transact_migration_classified(
    projects_path: &Path,
    plan: ValidatedMigrationPlanV1,
) -> Result<ProjectCatalogCommit, MigrationTransactionFailureV1> {
    transact_migration_classified_with_io(projects_path, plan, Arc::new(RealCatalogStoreIo))
}

fn transact_migration_classified_with_io(
    projects_path: &Path,
    plan: ValidatedMigrationPlanV1,
    io: Arc<dyn CatalogStoreIo>,
) -> Result<ProjectCatalogCommit, MigrationTransactionFailureV1> {
    match transact_migration_attempt_with_io(projects_path, plan.clone(), io.clone()) {
        Ok(commit) => Ok(commit),
        Err(MigrationAttemptFailureV1::NoDurableMutation(error)) => {
            Err(MigrationTransactionFailureV1 {
                error,
                disposition: MigrationMutationDispositionV1::NoDurableMutation,
            })
        }
        Err(MigrationAttemptFailureV1::Classify(error)) => {
            let disposition = classify_migration_failure(projects_path, &plan, io);
            Err(MigrationTransactionFailureV1 { error, disposition })
        }
    }
}

enum MigrationAttemptFailureV1 {
    NoDurableMutation(ProjectCatalogStoreError),
    Classify(ProjectCatalogStoreError),
}

#[cfg(test)]
impl MigrationAttemptFailureV1 {
    fn into_error(self) -> ProjectCatalogStoreError {
        match self {
            Self::NoDurableMutation(error) | Self::Classify(error) => error,
        }
    }
}

/// The checkout-id marker has TWO producer dialects: the runtime's
/// `ensure_checkout_id` writes the bare id, while migration installs
/// historically wrote a trailing newline; both readers trim. A byte-exact
/// comparison against either single dialect refuses the other producer's
/// legitimate marker (the rehearsal hit exactly that against a
/// daemon-written marker), so every migration-side comparison accepts both
/// shapes while installs write the runtime's bare shape.
fn checkout_marker_bytes_match(bytes: &[u8], id: &str) -> bool {
    bytes == id.as_bytes() || bytes.strip_suffix(b"\n") == Some(id.as_bytes())
}

fn transact_migration_attempt_with_io(
    projects_path: &Path,
    plan: ValidatedMigrationPlanV1,
    io: Arc<dyn CatalogStoreIo>,
) -> Result<ProjectCatalogCommit, MigrationAttemptFailureV1> {
    let paths = ProjectCatalogPaths::derive(projects_path)
        .map_err(MigrationAttemptFailureV1::NoDurableMutation)?;
    let exclusive = ProjectCatalogMigrationLock::try_acquire_exclusive(&paths.catalog)
        .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))
        .map_err(MigrationAttemptFailureV1::NoDurableMutation)?
        .ok_or_else(|| {
            MigrationAttemptFailureV1::NoDurableMutation(ProjectCatalogStoreError::new(
                "error.project_catalog_lifetime_lock_busy",
                "a compatible daemon or preflight still holds the lifetime lock",
            ))
        })?;
    let owner = ProjectCatalogTransactionOwner {
        paths,
        registry: ParticipantRegistry::Migration(Arc::new(plan.registry.clone())),
        io,
    };
    let _exclusive = exclusive;
    let _mutation_lock = owner
        .io
        .acquire_mutation_lock(&owner.paths.catalog)
        .map_err(MigrationAttemptFailureV1::NoDurableMutation)?;
    let _auxiliary_locks = owner
        .acquire_auxiliary_locks()
        .map_err(MigrationAttemptFailureV1::NoDurableMutation)?;
    if !owner
        .can_supersede_terminal_migration_rollback()
        .map_err(MigrationAttemptFailureV1::NoDurableMutation)?
    {
        owner
            .recover_locked()
            .map_err(MigrationAttemptFailureV1::Classify)?;
    }
    match owner.completed_migration_commit(&plan) {
        Ok(Some(commit)) => return Ok(commit),
        Ok(None) => {}
        Err(error) => return Err(MigrationAttemptFailureV1::Classify(error)),
    }
    let cleanup_plan = plan.clone();
    match owner.commit_migration_plan_locked(plan) {
        Ok(commit) => Ok(commit),
        Err(error) => match owner.cleanup_unjournaled_migration_attempt(&cleanup_plan) {
            Ok(true) => Err(MigrationAttemptFailureV1::NoDurableMutation(error)),
            Ok(false) | Err(_) => Err(MigrationAttemptFailureV1::Classify(error)),
        },
    }
}

fn classify_migration_failure(
    projects_path: &Path,
    plan: &ValidatedMigrationPlanV1,
    io: Arc<dyn CatalogStoreIo>,
) -> MigrationMutationDispositionV1 {
    let Ok(paths) = ProjectCatalogPaths::derive(projects_path) else {
        return MigrationMutationDispositionV1::NoDurableMutation;
    };
    let Ok(Some(_exclusive)) = ProjectCatalogMigrationLock::try_acquire_exclusive(&paths.catalog)
    else {
        return MigrationMutationDispositionV1::RetryExactPlanRequired;
    };
    let owner = ProjectCatalogTransactionOwner {
        paths,
        registry: ParticipantRegistry::Migration(Arc::new(plan.registry.clone())),
        io,
    };
    let Ok(_mutation_lock) = owner.io.acquire_mutation_lock(&owner.paths.catalog) else {
        return MigrationMutationDispositionV1::RetryExactPlanRequired;
    };
    let Ok(_auxiliary_locks) = owner.acquire_auxiliary_locks() else {
        return MigrationMutationDispositionV1::RetryExactPlanRequired;
    };
    if owner.recover_locked().is_err() {
        return MigrationMutationDispositionV1::RetryExactPlanRequired;
    }
    match owner.read_journal_locked() {
        Ok(Some(journal))
            if journal.kind == TransactionKindV1::V1Migration
                && journal.transaction_id == plan.journal.transaction_id =>
        {
            match (journal.state, journal.outcome) {
                (TransactionStateV1::Committed, Some(TransactionOutcomeV1::Committed)) => {
                    MigrationMutationDispositionV1::RecoveredToCommittedState
                }
                (TransactionStateV1::Committed, Some(TransactionOutcomeV1::RolledBack)) => {
                    MigrationMutationDispositionV1::RecoveredToOldState
                }
                _ => MigrationMutationDispositionV1::RetryExactPlanRequired,
            }
        }
        Ok(None) => match migration_plan_artifacts_exist_locked(&owner, plan) {
            Ok(false) => MigrationMutationDispositionV1::NoDurableMutation,
            Ok(true) | Err(_) => MigrationMutationDispositionV1::RetryExactPlanRequired,
        },
        _ => MigrationMutationDispositionV1::RetryExactPlanRequired,
    }
}

fn migration_plan_artifacts_exist_locked(
    owner: &ProjectCatalogTransactionOwner,
    plan: &ValidatedMigrationPlanV1,
) -> ProjectCatalogStoreResult<bool> {
    if path_exists_nofollow(&owner.paths.stage_dir)?
        || path_exists_nofollow(&owner.paths.backup_dir)?
    {
        return Ok(true);
    }
    let ParticipantRegistry::Migration(registry) = &owner.registry else {
        return Ok(true);
    };
    for participant in &plan.journal.participants {
        for (root, image) in [
            (&owner.paths.backup_dir, &participant.old),
            (&owner.paths.stage_dir, &participant.new),
        ] {
            let ExpectedImageV1::Present { artifact_name, .. } = image else {
                continue;
            };
            if owner
                .io
                .read_regular_nofollow(
                    &root.join(artifact_name.as_str()),
                    participant.role.max_bytes(),
                )?
                .is_some()
            {
                return Ok(true);
            }
        }
    }
    for asset in &plan.journal.immutable_assets {
        let target = registry.immutable_target(&asset.role, &asset.validated_name);
        if owner
            .io
            .read_regular_nofollow(&target, asset.role.max_bytes())?
            .is_some()
        {
            return Ok(true);
        }
    }
    for action in &plan.journal.monotonic_checkout_identity_actions {
        let Some(target) = registry.checkout_identity_target(&action.observation_id) else {
            continue;
        };
        if owner.io.read_regular_nofollow(&target, 128)?.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
fn transact_migration_with_io(
    projects_path: &Path,
    plan: ValidatedMigrationPlanV1,
    io: Arc<dyn CatalogStoreIo>,
) -> ProjectCatalogStoreResult<ProjectCatalogCommit> {
    transact_migration_attempt_with_io(projects_path, plan, io)
        .map_err(MigrationAttemptFailureV1::into_error)
}

#[cfg(test)]
fn recover_migration_with_io(
    projects_path: &Path,
    registry: MigrationParticipantRegistry,
    io: Arc<dyn CatalogStoreIo>,
) -> ProjectCatalogStoreResult<()> {
    let paths = ProjectCatalogPaths::derive(projects_path)?;
    let _exclusive = ProjectCatalogMigrationLock::try_acquire_exclusive(&paths.catalog)
        .map_err(|error| io_error("acquire lifetime lock for", &paths.catalog, error))?
        .ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_lifetime_lock_busy",
                "a compatible daemon or preflight still holds the lifetime lock",
            )
        })?;
    let owner = ProjectCatalogTransactionOwner {
        paths,
        registry: ParticipantRegistry::Migration(Arc::new(registry)),
        io,
    };
    let _mutation_lock = owner.io.acquire_mutation_lock(&owner.paths.catalog)?;
    let _auxiliary_locks = owner.acquire_auxiliary_locks()?;
    owner.recover_locked()
}

impl ProjectCatalogTransactionOwner {
    fn acquire_auxiliary_locks(&self) -> ProjectCatalogStoreResult<Vec<StoreLockGuard>> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Ok(Vec::new());
        };
        let mut guards = Vec::new();
        for store_path in registry.auxiliary_store_paths() {
            guards.push(self.io.acquire_mutation_lock(&store_path)?);
        }
        Ok(guards)
    }

    fn verify_live_code_source_snapshot(
        &self,
        snapshot: &MigrationCodeSourceSnapshotDraftV1,
        catalog: &CatalogSnapshotV2,
        post_images: &BTreeMap<ParticipantRoleV1, Option<Vec<u8>>>,
    ) -> ProjectCatalogStoreResult<()> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "code-source snapshot verification requires the complete registry",
            ));
        };
        let stale = |detail: &str| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_migration_inventory_stale",
                format!("code-source inventory changed after capture: {detail}"),
            )
        };
        let inventory_scopes = migration_inventory_scopes(
            catalog,
            post_images
                .iter()
                .map(|(role, bytes)| (role, bytes.as_deref())),
        )?;
        let current = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &inventory_scopes,
        )
        .map_err(|error| stale(&error.to_string()))?;
        if current.canonical_sha256 != snapshot.legacy_inventory.canonical_sha256 {
            return Err(stale("canonical legacy source row set differs"));
        }
        Ok(())
    }

    fn prevalidate_checkout_bindings_live(
        &self,
        attachments: &AttachmentSnapshotV1,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout binding verification requires the complete registry",
            ));
        };
        validate_checkout_bindings(
            registry,
            attachments,
            &journal.monotonic_checkout_identity_actions,
        )?;
        let actions = journal
            .monotonic_checkout_identity_actions
            .iter()
            .map(|action| (action.observation_id.as_str(), action))
            .collect::<BTreeMap<_, _>>();
        for (observation_id, target) in &registry.checkout_identity_markers {
            let root = registry
                .checkout_root(observation_id)
                .expect("validated registry target has a checkout root");
            let checkout_id = attachments
                .attachments
                .values()
                .find(|attachment| Path::new(&attachment.checkout_dir) == root)
                .map(|attachment| attachment.checkout_id.as_str())
                .expect("validated checkout binding has an attachment");
            let actual = self.io.read_regular_nofollow(target, 128)?;
            if let Some(action) = actions.get(observation_id.as_str()) {
                match actual.as_deref() {
                    None | Some([]) => {}
                    Some(bytes) if checkout_marker_bytes_match(bytes, &action.planned_id) => {}
                    Some(_) => {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_inventory_stale",
                            "planned checkout identity target changed after capture",
                        ));
                    }
                }
            } else {
                if !actual
                    .as_deref()
                    .is_some_and(|bytes| checkout_marker_bytes_match(bytes, checkout_id))
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_inventory_stale",
                        "existing checkout identity disagrees with the attachment snapshot",
                    ));
                }
            }
        }
        Ok(())
    }

    fn completed_migration_commit(
        &self,
        plan: &ValidatedMigrationPlanV1,
    ) -> ProjectCatalogStoreResult<Option<ProjectCatalogCommit>> {
        let Some(catalog_bytes) = self
            .io
            .read_regular_nofollow(&self.paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?
        else {
            return Ok(None);
        };
        if decode_legacy_project_store(&catalog_bytes).is_ok() {
            return Ok(None);
        }
        let catalog = decode_catalog_snapshot(&catalog_bytes).map_err(contract_error)?;
        let CatalogOriginV2::MigratedV1 { transaction_id } = &catalog.origin else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_inventory_stale",
                "migration target already contains an unrelated fresh v2 catalog",
            ));
        };
        if transaction_id != &plan.journal.transaction_id {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_inventory_stale",
                "migration target belongs to a different transaction",
            ));
        }
        let marker_bytes = self
            .io
            .read_regular_nofollow(&self.paths.migration_marker, MAX_MARKER_BYTES)?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "completed migration catalog lacks its retained marker",
                )
            })?;
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")?;
        verify_migration_marker_journal_binding(&marker, &marker_bytes, &plan.journal)?;
        let planned_marker_bytes = plan
            .post_images
            .get(&ParticipantRoleV1::MigrationMarker)
            .and_then(Option::as_deref)
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "validated migration plan lacks its marker post-image",
                )
            })?;
        let planned_marker_hash = plan
            .journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::MigrationMarker)
            .and_then(|participant| participant.new.sha256());
        if marker.transaction_id != plan.journal.transaction_id
            || Some(&marker.plan_hash) != plan.journal.plan_hash.as_ref()
            || Some(&marker.report_artifact_sha256) != plan.journal.report_artifact_sha256.as_ref()
            || Some(&marker.resolution_artifact_sha256)
                != plan.journal.resolution_artifact_sha256.as_ref()
            || marker.migration_epoch != plan.journal.new_epoch
            || marker_bytes != planned_marker_bytes
            || planned_marker_hash != Some(&sha256(&marker_bytes))
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "retained migration marker does not prove the requested plan",
            ));
        }
        self.verify_immutable_assets(&plan.journal)?;
        let state = self.verify_current_migration_state(&plan.journal)?;
        Ok(Some(ProjectCatalogCommit {
            epoch: state.epoch,
            catalog_sha256: state.catalog_sha256.to_string(),
            attachments_sha256: state.attachments_sha256.to_string(),
        }))
    }

    fn verify_current_migration_state(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<ProjectCatalogState> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "current migration verification requires the complete registry",
            ));
        };
        let state = self.read_strict_pair_locked()?;
        let expected_collision_generations = journal
            .resolved_quarantine_bindings
            .as_ref()
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "migration journal lacks canonical quarantine bindings",
                )
            })?
            .iter()
            .map(|(project_id, generation_id)| (project_id.clone(), generation_id.to_string()))
            .collect::<BTreeSet<_>>();
        let current = enumerate_current_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_catalog_scopes(&state.catalog),
            &BTreeSet::new(),
            &expected_collision_generations,
        )
        .map_err(|error| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                error.to_string(),
            )
        })?;
        for selection in &current.effective_manifest.selections {
            let project = state
                .catalog
                .projects
                .get(&selection.project_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "current source selection project is absent from catalog",
                    )
                })?;
            if project.scope != ProjectScope::Published(selection.published_scope.clone()) {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current source selection and catalog scope disagree",
                ));
            }
        }
        for pending in &current.collision_pending {
            let project = state
                .catalog
                .projects
                .get(&pending.project_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "current collision project is absent from catalog",
                    )
                })?;
            let participant_exists = journal.participants.iter().any(|candidate| {
                candidate.role
                    == (ParticipantRoleV1::CollisionRetirement {
                        project_id: pending.project_id.clone(),
                    })
            });
            if project.scope != ProjectScope::LegacyLocal || !participant_exists {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current collision lifecycle lacks its LegacyLocal catalog owner and transaction evidence",
                ));
            }
        }
        let marker_bytes = self
            .io
            .read_regular_nofollow(&self.paths.migration_marker, MAX_MARKER_BYTES)?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current migration marker is missing",
                )
            })?;
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")?;
        verify_migration_marker_journal_binding(&marker, &marker_bytes, journal)?;
        for participant in &journal.participants {
            let ParticipantRoleV1::CollisionRetirement { project_id } = &participant.role else {
                continue;
            };
            let lifecycle = current
                .collision_pending
                .iter()
                .find(|row| row.project_id == *project_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "historical collision lacks its durable lifecycle record",
                    )
                })?;
            let previous_lifecycle = match &participant.old {
                ExpectedImageV1::Absent {} => None,
                ExpectedImageV1::Present {
                    sha256: expected,
                    artifact_name,
                } => {
                    let bytes = self
                        .io
                        .read_regular_nofollow(
                            &self.paths.backup_dir.join(artifact_name.as_str()),
                            MAX_CODE_SOURCE_COLLISION_RETIREMENT_BYTES,
                        )?
                        .ok_or_else(|| {
                            ProjectCatalogStoreError::new(
                                "error.project_catalog_migration_incomplete",
                                "historical collision lifecycle backup is missing",
                            )
                        })?;
                    if sha256(&bytes) != *expected {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision lifecycle backup hash disagrees",
                        ));
                    }
                    Some(
                        decode_collision_retirement_pending_for_migration(&bytes).map_err(
                            |error| {
                                ProjectCatalogStoreError::new(
                                    "error.project_catalog_migration_incomplete",
                                    error.to_string(),
                                )
                            },
                        )?,
                    )
                }
            };
            if let Some(previous) = &previous_lifecycle {
                lifecycle
                    .record
                    .validate_descendant_from(previous)
                    .map_err(|error| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            format!(
                                "current collision lifecycle rewrites installed evidence: {error}"
                            ),
                        )
                    })?;
            }
            let expected_generation_ids = expected_collision_generations
                .iter()
                .filter(|(owner, _)| owner == project_id)
                .map(|(_, generation_id)| generation_id.clone())
                .collect::<BTreeSet<_>>();
            if lifecycle
                .record
                .entries
                .keys()
                .cloned()
                .collect::<BTreeSet<_>>()
                != expected_generation_ids
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "historical collision lifecycle membership changed",
                ));
            }
            let activation_participant = journal.participants.iter().find(|candidate| {
                candidate.role
                    == (ParticipantRoleV1::Activation {
                        project_id: project_id.clone(),
                    })
            });
            let exact_entries = lifecycle
                .record
                .entries
                .iter()
                .filter_map(|(generation_id, entry)| {
                    entry
                        .exact_selector()
                        .map(|selector| (generation_id, entry, selector))
                })
                .collect::<Vec<_>>();
            let old_activation = match exact_entries.as_slice() {
                [] => {
                    if activation_participant.is_some() {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "retained historical collision acquired activation authority",
                        ));
                    }
                    None
                }
                [(generation_id, entry, selector)] => {
                    let activation_participant = activation_participant.ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "active historical collision lacks activation evidence",
                        )
                    })?;
                    let ExpectedImageV1::Present {
                        sha256: expected,
                        artifact_name,
                    } = &activation_participant.old
                    else {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision activation evidence is absent",
                        ));
                    };
                    let old_activation_bytes = self
                        .io
                        .read_regular_nofollow(
                            &self.paths.backup_dir.join(artifact_name.as_str()),
                            MAX_CODE_SOURCE_ACTIVATION_BYTES,
                        )?
                        .ok_or_else(|| {
                            ProjectCatalogStoreError::new(
                                "error.project_catalog_migration_incomplete",
                                "historical collision activation backup is missing",
                            )
                        })?;
                    if sha256(&old_activation_bytes) != *expected {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision activation backup hash disagrees",
                        ));
                    }
                    let old_activation = decode_activation_v1_for_migration(&old_activation_bytes)
                        .map_err(|error| {
                            ProjectCatalogStoreError::new(
                                "error.project_catalog_migration_incomplete",
                                error.to_string(),
                            )
                        })?;
                    if old_activation.selector != *selector
                        || old_activation.generation_id != generation_id.as_str()
                        || old_activation.snapshot_id != entry.snapshot_id
                        || current
                            .activations
                            .iter()
                            .any(|row| row.project_id == *project_id)
                    {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical active collision rewrites or reactivates selector evidence",
                        ));
                    }
                    Some(old_activation)
                }
                _ => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "historical collision has multiple active selector entries",
                    ));
                }
            };
            for (generation_id, entry) in &lifecycle.record.entries {
                let stored_participant = journal
                    .participants
                    .iter()
                    .find(|candidate| match &candidate.role {
                        ParticipantRoleV1::StoredGenerationMetadata {
                            project_id: owner,
                            published_scope,
                            generation_id: stored_generation_id,
                        } => {
                            owner == project_id
                                && published_scope == &entry.former_scope
                                && stored_generation_id.as_str() == generation_id
                        }
                        _ => false,
                    })
                    .ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision lacks its scope-bearing generation evidence",
                        )
                    })?;
                let ExpectedImageV1::Present {
                    sha256: expected_stored,
                    artifact_name: stored_artifact_name,
                } = &stored_participant.old
                else {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "historical collision stored evidence is absent",
                    ));
                };
                let old_stored_bytes = self
                    .io
                    .read_regular_nofollow(
                        &self.paths.backup_dir.join(stored_artifact_name.as_str()),
                        MAX_CODE_SOURCE_GENERATION_METADATA_BYTES,
                    )?
                    .ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision stored backup is missing",
                        )
                    })?;
                if sha256(&old_stored_bytes) != *expected_stored {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "historical collision stored backup hash disagrees",
                    ));
                }
                let old_stored = decode_stored_generation_v1_for_migration(&old_stored_bytes)
                    .map_err(|error| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            error.to_string(),
                        )
                    })?;
                let generation_id_typed = Sha256Hex::parse(generation_id.clone())
                    .expect("validated lifecycle generation id");
                let _manifest = journal
                    .immutable_assets
                    .iter()
                    .find(|asset| {
                        asset.role
                            == (ImmutableAssetRoleV1::CollectedGenerationManifest {
                                published_scope: entry.former_scope.clone(),
                                generation_id: generation_id_typed.clone(),
                            })
                    })
                    .ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "historical collision lacks its immutable manifest evidence",
                        )
                    })?;
                let is_active_entry = old_activation
                    .as_ref()
                    .is_some_and(|activation| activation.generation_id == generation_id.as_str());
                if old_stored.generation_id != generation_id.as_str()
                    || old_stored.descriptor.scope != entry.former_scope
                    || old_stored.descriptor.manifest_sha256 != entry.manifest_sha256
                    || (previous_lifecycle.is_none()
                        && (entry.inventory_hash != marker.inventory_sha256.as_str()
                            || entry.plan_hash != marker.plan_hash.as_str()))
                    || (is_active_entry && entry.exact_selector().is_none())
                    || (!is_active_entry
                        && entry.selector_evidence
                            != CollisionRetirementSelectorEvidenceV1::NoDurableSelector)
                    || (!is_active_entry
                        && old_stored.state == bbox_code_source::GenerationState::Active)
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "current collision lifecycle rewrites historical generation evidence",
                    ));
                }
                let work = current.collision_work.iter().find(|work| {
                    work.record.project_id == *project_id
                        && work.record.generation_id == generation_id.as_str()
                });
                if entry.state == CollisionRetirementLifecycleStateV1::Queued && work.is_none() {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "queued collision lifecycle entry lacks subordinate work",
                    ));
                }
            }
        }

        let limits = AcceptedPublicationLimits::default();
        let publisher_backup = journal
            .immutable_assets
            .iter()
            .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyPublisherRefBackup);
        match journal.publisher_ref_source.as_ref().ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration journal lacks publisher source presence evidence",
            )
        })? {
            MigrationPublisherSourceEvidenceV1::Missing { .. } if publisher_backup.is_none() => {}
            MigrationPublisherSourceEvidenceV1::Present { sha256: expected } => {
                let publisher_backup = publisher_backup.ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "present publisher source lacks retained backup",
                    )
                })?;
                let publisher_source = self
                    .io
                    .read_regular_nofollow(
                        &registry.immutable_target(
                            &publisher_backup.role,
                            &publisher_backup.validated_name,
                        ),
                        MAX_LEGACY_PUBLISHER_REF_SOURCE_BYTES,
                    )?
                    .ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_migration_incomplete",
                            "retained publisher source backup is missing",
                        )
                    })?;
                if sha256(&publisher_source) != *expected {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "retained publisher source backup hash disagrees",
                    ));
                }
                validate_publisher_source_binding(
                    &publisher_source,
                    &journal.publisher_pins,
                    "retained migration source",
                )?;
            }
            MigrationPublisherSourceEvidenceV1::Missing { .. } => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "missing publisher source has a fabricated backup",
                ));
            }
        }
        for pin in &journal.publisher_pins {
            let project = state.catalog.projects.get(&pin.project_id).ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current publisher pin project is absent from catalog",
                )
            })?;
            if project.scope != ProjectScope::Published(pin.expected_scope.clone()) {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current publisher pin and catalog scope disagree",
                ));
            }
            let pointer_bytes = self.io.read_regular_nofollow(
                &registry.accepted_publication_paths.pointer(&pin.project_id),
                MAX_ACCEPTED_PUBLICATION_POINTER_BYTES,
            )?;
            let seeded = journal.publisher_dispositions.iter().any(|disposition| {
                matches!(
                    disposition,
                    PublisherDispositionEvidenceV1::SeedG1 {
                        observation_id,
                        ..
                    } if observation_id == &pin.observation_id
                )
            });
            let Some(pointer_bytes) = pointer_bytes else {
                if seeded {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "seeded publisher pin lost its accepted publication pointer",
                    ));
                }
                continue;
            };
            let pointer = decode_pointer_v1(&pointer_bytes, &limits).map_err(|error| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    error.to_string(),
                )
            })?;
            if pointer.project_id != pin.project_id
                || pointer.accepted_scope != pin.expected_scope
                || pointer.full_ref != pin.full_ref
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current accepted publication rewrites pinned publisher authority",
                ));
            }
            let attachment = state
                .attachments
                .attachments
                .get(&pointer.attachment_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "current accepted publication attachment is absent",
                    )
                })?;
            if attachment.project_id != pin.project_id
                || attachment.status != AttachmentStatus::Attached
                || attachment.validated_scope.as_ref() != Some(&pin.expected_scope)
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "current accepted publication attachment lacks pinned authority",
                ));
            }
            let generation_path = registry
                .accepted_publication_paths
                .generation(&pin.project_id, &pointer.accepted_generation);
            let generation_bytes = self
                .io
                .read_regular_nofollow(&generation_path, MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES)?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "current accepted publication generation is missing",
                    )
                })?;
            verify_pointer_generation_v1(&pointer, &generation_bytes, &limits).map_err(
                |error| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        error.to_string(),
                    )
                },
            )?;
        }
        self.verify_immutable_assets(journal)?;
        Ok(state)
    }

    fn can_supersede_terminal_migration_rollback(&self) -> ProjectCatalogStoreResult<bool> {
        let Some(journal) = self.read_journal_locked()? else {
            return Ok(false);
        };
        if journal.kind != TransactionKindV1::V1Migration
            || journal.state != TransactionStateV1::Committed
            || journal.outcome != Some(TransactionOutcomeV1::RolledBack)
        {
            return Ok(false);
        }
        // The committed rollback proved exact restoration before publishing this
        // terminal journal. A corrected attempt may legitimately start from
        // subsequently changed legacy inputs, so only their structural legacy
        // shape remains authoritative here. The incoming plan rebinds the live
        // bytes and complete migration inventory under the same locks.
        let catalog = self
            .io
            .read_regular_nofollow(&self.paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?;
        let attachments = self
            .io
            .read_regular_nofollow(&self.paths.attachments, MAX_PROJECT_CATALOG_BYTES)?;
        if attachments.is_some()
            || catalog
                .as_deref()
                .is_some_and(|bytes| decode_legacy_project_store(bytes).is_err())
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                "terminal rollback no longer has a structural legacy catalog state",
            ));
        }
        for participant in &journal.participants {
            if participant.old.sha256().is_some()
                && !self.artifact_available(
                    &self.paths.backup_dir,
                    &participant.old,
                    participant.role.max_bytes(),
                )?
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "terminal rollback lacks its retained old participant evidence",
                ));
            }
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "terminal migration rollback requires the complete registry",
            ));
        };
        for action in &journal.monotonic_checkout_identity_actions {
            let target = registry
                .checkout_identity_target(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "terminal rollback checkout action is unregistered",
                    )
                })?;
            let bytes = self
                .io
                .read_regular_nofollow(&target, 128)?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "terminal rollback lost its monotonic checkout identity",
                    )
                })?;
            if !valid_checkout_identity_bytes(&bytes) {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "terminal rollback checkout identity is malformed",
                ));
            }
        }
        Ok(true)
    }

    fn preserve_superseded_rollback_journal(&self) -> ProjectCatalogStoreResult<()> {
        let Some(journal) = self.read_journal_locked()? else {
            return Ok(());
        };
        if journal.kind != TransactionKindV1::V1Migration
            || journal.state != TransactionStateV1::Committed
            || journal.outcome != Some(TransactionOutcomeV1::RolledBack)
        {
            return Ok(());
        }
        let bytes = encode_bounded_json(&journal, MAX_JOURNAL_BYTES, "transaction journal")?;
        let hash = sha256(&bytes);
        let name = ValidatedBasename::parse(format!(
            "{}.rollback-journal.{}.json",
            journal.transaction_id, hash
        ))?;
        self.write_artifact(
            &self.paths.backup_dir.join(name.as_str()),
            &bytes,
            hash,
            FaultPoint::BackupWrite,
            FaultPoint::BackupFsync,
        )
    }

    fn preserve_committed_migration_journal(
        &self,
        journal: &mut ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        journal.validate()?;
        if journal.kind != TransactionKindV1::V1Migration
            || journal.state != TransactionStateV1::Committed
            || journal.outcome != Some(TransactionOutcomeV1::Committed)
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "only a committed migration journal can become retained migration evidence",
            ));
        }
        if let Some(existing_bytes) = self
            .io
            .read_regular_nofollow(&self.paths.migration_receipt, MAX_JOURNAL_BYTES)?
        {
            let existing: ProjectCatalogTransactionJournalV1 =
                decode_bounded_json(&existing_bytes, MAX_JOURNAL_BYTES, "migration receipt")?;
            existing.validate()?;
            let mut expected = journal.clone();
            expected.committed_at = existing.committed_at;
            if existing != expected {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_artifact_collision",
                    "durable migration receipt disagrees with the committed transaction",
                ));
            }
            *journal = existing;
        }
        let bytes = encode_bounded_json(journal, MAX_JOURNAL_BYTES, "transaction journal")?;
        let hash = sha256(&bytes);
        self.write_artifact(
            &self.paths.migration_receipt,
            &bytes,
            hash,
            FaultPoint::BackupWrite,
            FaultPoint::BackupFsync,
        )
    }

    fn remove_provisional_migration_receipt_for_rollback(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        let Some(bytes) = self
            .io
            .read_regular_nofollow(&self.paths.migration_receipt, MAX_JOURNAL_BYTES)?
        else {
            return Ok(());
        };
        let existing: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "migration receipt")?;
        existing.validate()?;
        let mut expected = journal.clone();
        expected.state = TransactionStateV1::Committed;
        expected.outcome = Some(TransactionOutcomeV1::Committed);
        expected.committed_at = existing.committed_at;
        if existing != expected {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_artifact_collision",
                "provisional migration receipt disagrees with the rolling-back transaction",
            ));
        }
        self.io.remove_regular_exact(
            &self.paths.migration_receipt,
            &sha256(&bytes),
            MAX_JOURNAL_BYTES,
        )
    }

    fn migration_journal_for_marker_locked(
        &self,
        marker: &ProjectCatalogMigrationMarkerV1,
    ) -> ProjectCatalogStoreResult<ProjectCatalogTransactionJournalV1> {
        if let Some(active) = self.read_journal_locked()?
            && active.kind == TransactionKindV1::V1Migration
            && active.transaction_id == marker.transaction_id
        {
            return Ok(active);
        }
        let bytes = self
            .io
            .read_regular_nofollow(&self.paths.migration_receipt, MAX_JOURNAL_BYTES)?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_incomplete",
                    "migration verification lacks its durable migration receipt",
                )
            })?;
        let journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "migration receipt")?;
        journal.validate()?;
        if journal.kind != TransactionKindV1::V1Migration
            || journal.state != TransactionStateV1::Committed
            || journal.outcome != Some(TransactionOutcomeV1::Committed)
            || journal.transaction_id != marker.transaction_id
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration receipt does not prove the marker transaction",
            ));
        }
        Ok(journal)
    }

    fn committed_migration_journal_for_marker_locked(
        &self,
        marker: &ProjectCatalogMigrationMarkerV1,
    ) -> ProjectCatalogStoreResult<ProjectCatalogTransactionJournalV1> {
        let journal = self.migration_journal_for_marker_locked(marker)?;
        if journal.state != TransactionStateV1::Committed
            || journal.outcome != Some(TransactionOutcomeV1::Committed)
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker lacks committed migration evidence",
            ));
        }
        Ok(journal)
    }

    fn read_strict_pair_locked(&self) -> ProjectCatalogStoreResult<ProjectCatalogState> {
        let catalog_bytes = self
            .io
            .read_regular_nofollow(&self.paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?;
        let attachment_bytes = self
            .io
            .read_regular_nofollow(&self.paths.attachments, MAX_PROJECT_CATALOG_BYTES)?;
        let (catalog_bytes, attachment_bytes) = match (catalog_bytes, attachment_bytes) {
            (None, None) => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_not_initialized",
                    "strict catalog and attachment snapshots are both missing",
                ));
            }
            (Some(catalog), None) => {
                if decode_legacy_project_store(&catalog).is_ok() {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_legacy_store_requires_migration",
                        "projects.json contains the legacy v1 project store",
                    ));
                }
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_incomplete_pair",
                    "the strict attachment snapshot is missing",
                ));
            }
            (None, Some(_)) => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_incomplete_pair",
                    "the strict catalog snapshot is missing",
                ));
            }
            (Some(catalog), Some(attachments)) => (catalog, attachments),
        };

        let catalog = match decode_catalog_snapshot(&catalog_bytes) {
            Ok(catalog) => catalog,
            Err(error) => {
                if decode_legacy_project_store(&catalog_bytes).is_ok() {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_legacy_store_requires_migration",
                        "projects.json contains the legacy v1 project store",
                    ));
                }
                return Err(contract_error(error));
            }
        };
        let attachments = decode_attachment_snapshot(&attachment_bytes).map_err(contract_error)?;
        validate_catalog_attachments(&catalog, &attachments).map_err(contract_error)?;
        if catalog.epoch == 0 {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_zero_epoch",
                "strict project catalog snapshots have epoch zero",
            ));
        }
        self.verify_origin_marker_locked(&catalog)?;

        Ok(ProjectCatalogState {
            epoch: catalog.epoch,
            catalog: Arc::new(catalog),
            attachments: Arc::new(attachments),
            catalog_sha256: sha256(&catalog_bytes),
            attachments_sha256: sha256(&attachment_bytes),
        })
    }

    fn verify_origin_marker_locked(
        &self,
        catalog: &CatalogSnapshotV2,
    ) -> ProjectCatalogStoreResult<()> {
        let marker_bytes = self
            .io
            .read_regular_nofollow(&self.paths.migration_marker, MAX_MARKER_BYTES)?;
        match (&catalog.origin, marker_bytes) {
            (CatalogOriginV2::FreshV2 {}, None) => Ok(()),
            (CatalogOriginV2::FreshV2 {}, Some(_)) => Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "fresh v2 catalog unexpectedly has a migration marker",
            )),
            (CatalogOriginV2::MigratedV1 { transaction_id }, Some(bytes)) => {
                let marker: ProjectCatalogMigrationMarkerV1 =
                    decode_bounded_json(&bytes, MAX_MARKER_BYTES, "migration marker")?;
                marker.validate()?;
                if &marker.transaction_id != transaction_id {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "migration marker transaction does not match catalog origin",
                    ));
                }
                let journal = self.migration_journal_for_marker_locked(&marker)?;
                verify_migration_marker_journal_binding(&marker, &bytes, &journal)?;
                Ok(())
            }
            (CatalogOriginV2::MigratedV1 { .. }, None) => Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migrated v2 catalog lacks its committed migration marker",
            )),
        }
    }

    fn commit_regular_pair_locked(
        &self,
        old: Option<&ProjectCatalogState>,
        new: &PreparedPair,
    ) -> ProjectCatalogStoreResult<()> {
        let prior_journal = self.read_journal_locked()?;
        self.io.create_private_dir_nofollow(&self.paths.stage_dir)?;
        self.io
            .create_private_dir_nofollow(&self.paths.backup_dir)?;
        if let Some(prior) = prior_journal.as_ref()
            && prior.kind == TransactionKindV1::V1Migration
            && prior.state == TransactionStateV1::Committed
            && prior.outcome == Some(TransactionOutcomeV1::Committed)
        {
            let mut retained = prior.clone();
            self.preserve_committed_migration_journal(&mut retained)?;
        }

        let transaction_id = ProjectCatalogTransactionId::mint();
        let mut participants = Vec::with_capacity(2);
        for (role, target, new_bytes, new_hash) in [
            (
                ParticipantRoleV1::Catalog,
                &self.paths.catalog,
                new.catalog_bytes.as_slice(),
                &new.catalog_sha256,
            ),
            (
                ParticipantRoleV1::Attachments,
                &self.paths.attachments,
                new.attachment_bytes.as_slice(),
                &new.attachments_sha256,
            ),
        ] {
            let old_image = match self
                .io
                .read_regular_nofollow(target, MAX_PROJECT_CATALOG_BYTES)?
            {
                Some(bytes) => {
                    let hash = sha256(&bytes);
                    let backup_name =
                        artifact_name(&transaction_id, &role, &hash, ArtifactKind::Backup)?;
                    let backup_path = self.paths.backup_dir.join(backup_name.as_str());
                    self.write_artifact(
                        &backup_path,
                        &bytes,
                        hash.clone(),
                        FaultPoint::BackupWrite,
                        FaultPoint::BackupFsync,
                    )?;
                    ExpectedImageV1::Present {
                        sha256: hash,
                        artifact_name: backup_name,
                    }
                }
                None => ExpectedImageV1::Absent {},
            };
            let stage_name = artifact_name(&transaction_id, &role, new_hash, ArtifactKind::Stage)?;
            let stage_path = self.paths.stage_dir.join(stage_name.as_str());
            self.write_artifact(
                &stage_path,
                new_bytes,
                new_hash.clone(),
                FaultPoint::StageWrite,
                FaultPoint::StageFsync,
            )?;
            participants.push(TransactionParticipantV1 {
                role,
                old: old_image,
                new: ExpectedImageV1::Present {
                    sha256: new_hash.clone(),
                    artifact_name: stage_name,
                },
            });
        }

        let now = unix_timestamp()?;
        let mut journal = ProjectCatalogTransactionJournalV1 {
            version: JOURNAL_VERSION,
            transaction_id,
            kind: TransactionKindV1::RegularPair,
            state: TransactionStateV1::Prepared,
            outcome: None,
            plan_hash: None,
            report_artifact_sha256: None,
            resolution_artifact_sha256: None,
            legacy_project_source: None,
            publisher_ref_source: None,
            publisher_pins: Vec::new(),
            publisher_dispositions: Vec::new(),
            resolved_quarantine_bindings: None,
            old_epoch: old.map_or(0, |state| state.epoch),
            new_epoch: new.catalog.epoch,
            participants,
            immutable_assets: Vec::new(),
            monotonic_checkout_identity_actions: Vec::new(),
            created_at: now,
            committed_at: None,
        };
        journal.validate()?;
        self.write_journal(&journal, FaultPoint::PreparedJournalWrite)?;
        if let Some(prior) = prior_journal.as_ref() {
            self.cleanup_obsolete_regular_evidence(prior)?;
        }

        for participant in &journal.participants {
            self.io.checkpoint(FaultPoint::ParticipantInstall)?;
            self.install_new_image(participant)?;
            self.io.checkpoint(FaultPoint::ParticipantInstall)?;
        }
        self.io.checkpoint(FaultPoint::CompletePlanVerify)?;
        self.verify_expected_pair(&journal, ExpectedSide::New)?;
        let installed = self.read_strict_pair_locked()?;
        if installed.epoch != journal.new_epoch
            || installed.catalog_sha256 != new.catalog_sha256
            || installed.attachments_sha256 != new.attachments_sha256
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_install_verification",
                "installed pair does not match prepared post-images",
            ));
        }
        self.io.checkpoint(FaultPoint::CompletePlanVerify)?;

        journal.state = TransactionStateV1::Committed;
        journal.outcome = Some(TransactionOutcomeV1::Committed);
        journal.committed_at = Some(unix_timestamp()?);
        journal.validate()?;
        self.write_journal(&journal, FaultPoint::CommittedJournalWrite)?;
        Ok(())
    }

    fn read_journal_locked(
        &self,
    ) -> ProjectCatalogStoreResult<Option<ProjectCatalogTransactionJournalV1>> {
        let Some(bytes) = self
            .io
            .read_regular_nofollow(&self.paths.journal, MAX_JOURNAL_BYTES)?
        else {
            return Ok(None);
        };
        let journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "transaction journal")?;
        journal.validate()?;
        Ok(Some(journal))
    }

    fn cleanup_obsolete_regular_evidence(
        &self,
        prior: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        prior.validate()?;
        if prior.kind != TransactionKindV1::RegularPair
            || prior.state != TransactionStateV1::Committed
            || prior.outcome != Some(TransactionOutcomeV1::Committed)
        {
            return Ok(());
        }
        self.verify_expected_pair(prior, ExpectedSide::New)?;
        for participant in &prior.participants {
            for (root, image) in [
                (&self.paths.backup_dir, &participant.old),
                (&self.paths.stage_dir, &participant.new),
            ] {
                let ExpectedImageV1::Present {
                    sha256,
                    artifact_name,
                } = image
                else {
                    continue;
                };
                self.io.checkpoint(FaultPoint::Cleanup)?;
                self.io.remove_regular_exact(
                    &root.join(artifact_name.as_str()),
                    sha256,
                    participant.role.max_bytes(),
                )?;
                self.io.checkpoint(FaultPoint::Cleanup)?;
            }
        }
        Ok(())
    }

    fn cleanup_unjournaled_migration_attempt(
        &self,
        plan: &ValidatedMigrationPlanV1,
    ) -> ProjectCatalogStoreResult<bool> {
        if self.read_journal_locked()?.is_some() {
            return Ok(false);
        }
        for participant in &plan.journal.participants {
            for (root, image) in [
                (&self.paths.backup_dir, &participant.old),
                (&self.paths.stage_dir, &participant.new),
            ] {
                let ExpectedImageV1::Present {
                    sha256,
                    artifact_name,
                } = image
                else {
                    continue;
                };
                let path = root.join(artifact_name.as_str());
                if path_exists_nofollow(&path)? {
                    self.io
                        .remove_regular_exact(&path, sha256, participant.role.max_bytes())?;
                }
            }
        }
        for asset in &plan.journal.immutable_assets {
            let Some(stage_name) = asset.stage_name.as_ref() else {
                continue;
            };
            let path = self.paths.stage_dir.join(stage_name.as_str());
            if path_exists_nofollow(&path)? {
                self.io
                    .remove_regular_exact(&path, &asset.sha256, asset.role.max_bytes())?;
            }
        }
        self.io.remove_empty_dir_nofollow(&self.paths.stage_dir)?;
        self.io.remove_empty_dir_nofollow(&self.paths.backup_dir)?;
        Ok(true)
    }

    #[allow(dead_code)] // P1-B apply seam consumed by P1-C.
    fn commit_migration_plan_locked(
        &self,
        plan: ValidatedMigrationPlanV1,
    ) -> ProjectCatalogStoreResult<ProjectCatalogCommit> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "migration transaction requires the complete registry",
            ));
        };

        // The complete live inventory is revalidated before this transaction
        // creates even a staging directory. A stale plan therefore has no
        // filesystem side effects and cannot poison a corrected attempt.
        let expected_publisher_source =
            plan.journal.publisher_ref_source.as_ref().ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "validated migration plan lacks its publisher source evidence",
                )
            })?;
        let publisher_source = self.io.read_regular_nofollow(
            &registry.legacy_publisher_ref_source,
            MAX_LEGACY_PUBLISHER_REF_SOURCE_BYTES,
        )?;
        let planned_publisher_source = plan
            .immutable_asset_bytes
            .get(&ImmutableAssetRoleV1::LegacyPublisherRefBackup);
        match (expected_publisher_source, publisher_source.as_deref()) {
            (MigrationPublisherSourceEvidenceV1::Missing { .. }, None)
                if planned_publisher_source.is_none() => {}
            (MigrationPublisherSourceEvidenceV1::Present { sha256: expected }, Some(bytes))
                if sha256(bytes) == *expected
                    && planned_publisher_source.is_some_and(|planned| planned == bytes) =>
            {
                validate_publisher_source_binding(
                    bytes,
                    &plan.journal.publisher_pins,
                    "live migration source",
                )?;
            }
            _ => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_inventory_stale",
                    "legacy publisher source presence or bytes changed after inventory",
                ));
            }
        }
        let catalog_bytes = plan
            .post_images
            .get(&ParticipantRoleV1::Catalog)
            .and_then(Option::as_deref)
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "validated migration plan lacks its catalog post-image",
                )
            })?;
        let planned_catalog = decode_catalog_snapshot(catalog_bytes).map_err(contract_error)?;
        let mut old_images = std::collections::BTreeMap::new();
        for participant in &plan.journal.participants {
            let target = self.target_for_role(&participant.role)?;
            let actual = self
                .io
                .read_regular_nofollow(&target, participant.role.max_bytes())?;
            let actual_hash = actual.as_ref().map(|bytes| sha256(bytes));
            if actual_hash.as_ref() != participant.old.sha256() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_inventory_stale",
                    "migration participant no longer matches its inventoried old image",
                ));
            }
            match (plan.post_images.get(&participant.role), &participant.new) {
                (
                    Some(Some(bytes)),
                    ExpectedImageV1::Present {
                        sha256: expected_hash,
                        ..
                    },
                ) if sha256(bytes) == *expected_hash => {}
                (Some(None), ExpectedImageV1::Absent {}) => {}
                _ => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "validated migration plan disagrees with its journal",
                    ));
                }
            }
            old_images.insert(participant.role.clone(), actual);
        }
        for asset in &plan.journal.immutable_assets {
            let target = registry.immutable_target(&asset.role, &asset.validated_name);
            let planned_bytes = plan.immutable_asset_bytes.get(&asset.role);
            if target.file_name().and_then(|name| name.to_str())
                != Some(asset.validated_name.as_str())
                || match asset.mode {
                    ImmutableAssetModeV1::Installable => {
                        !planned_bytes.is_some_and(|bytes| sha256(bytes) == asset.sha256)
                    }
                    ImmutableAssetModeV1::PinnedExisting => planned_bytes.is_some(),
                }
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_migration_plan",
                    "validated migration immutable bytes or target disagree with the journal",
                ));
            }
            let existing = self
                .io
                .read_regular_nofollow(&target, asset.role.max_bytes())?;
            match (asset.mode, existing) {
                (_, Some(bytes)) if sha256(&bytes) != asset.sha256 => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_artifact_collision",
                        "immutable migration target has unexpected bytes",
                    ));
                }
                (ImmutableAssetModeV1::PinnedExisting, None) => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_inventory_stale",
                        "pinned immutable migration asset disappeared after inventory",
                    ));
                }
                _ => {}
            }
        }
        // Verify pinned bytes before replaying their semantic inventory. This
        // preserves the precise distinction between a disappeared pinned asset
        // and a present immutable target whose bytes collide with its hash.
        self.verify_live_code_source_snapshot(
            &plan.code_source_snapshot,
            &planned_catalog,
            &plan.post_images,
        )?;
        let immutable_evidence = plan
            .journal
            .immutable_assets
            .iter()
            .map(|asset| MigrationImmutableAssetEvidenceV1 {
                role: asset.role.clone(),
                mode: asset.mode,
                sha256: asset.sha256.clone(),
                validated_name: asset.validated_name.clone(),
            })
            .collect::<Vec<_>>();
        let planned_catalog = decode_catalog_snapshot(
            plan.post_images
                .get(&ParticipantRoleV1::Catalog)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "validated migration plan lacks its catalog post-image",
                    )
                })?,
        )
        .map_err(contract_error)?;
        let planned_attachments = decode_attachment_snapshot(
            plan.post_images
                .get(&ParticipantRoleV1::Attachments)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "validated migration plan lacks its attachment post-image",
                    )
                })?,
        )
        .map_err(contract_error)?;
        let planned_marker: ProjectCatalogMigrationMarkerV1 = decode_bounded_json(
            plan.post_images
                .get(&ParticipantRoleV1::MigrationMarker)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "validated migration plan lacks its marker evidence",
                    )
                })?,
            MAX_MARKER_BYTES,
            "migration marker",
        )?;
        planned_marker.validate()?;
        validate_new_side_cross_roles(
            &planned_catalog,
            &planned_attachments,
            &plan.post_images,
            &immutable_evidence,
            &plan.immutable_asset_bytes,
            &plan.journal.publisher_pins,
            &plan.journal.publisher_dispositions,
            "error.project_catalog_invalid_migration_plan",
        )?;
        self.prevalidate_checkout_bindings_live(&planned_attachments, &plan.journal)?;
        self.prevalidate_monotonic_checkout_actions(&plan.journal)?;

        self.io.create_private_dir_nofollow(&self.paths.stage_dir)?;
        self.io
            .create_private_dir_nofollow(&self.paths.backup_dir)?;
        self.preserve_superseded_rollback_journal()?;

        for asset in &plan.journal.immutable_assets {
            if asset.mode != ImmutableAssetModeV1::Installable {
                continue;
            }
            let bytes = plan
                .immutable_asset_bytes
                .get(&asset.role)
                .expect("immutable plan bytes were revalidated");
            let stage_name = asset
                .stage_name
                .as_ref()
                .expect("installable immutable asset has a stage name");
            self.write_artifact(
                &self.paths.stage_dir.join(stage_name.as_str()),
                bytes,
                asset.sha256.clone(),
                FaultPoint::ImmutableAssetWrite,
                FaultPoint::ImmutableAssetFsync,
            )?;
        }

        for participant in &plan.journal.participants {
            let actual = old_images
                .get(&participant.role)
                .expect("all participants were revalidated");
            if let (
                Some(bytes),
                ExpectedImageV1::Present {
                    sha256,
                    artifact_name,
                },
            ) = (actual.as_ref(), &participant.old)
            {
                self.write_artifact(
                    &self.paths.backup_dir.join(artifact_name.as_str()),
                    bytes,
                    sha256.clone(),
                    FaultPoint::BackupWrite,
                    FaultPoint::BackupFsync,
                )?;
            }
            match (plan.post_images.get(&participant.role), &participant.new) {
                (
                    Some(Some(bytes)),
                    ExpectedImageV1::Present {
                        sha256: expected_hash,
                        artifact_name,
                    },
                ) if sha256(bytes) == *expected_hash => {
                    self.write_artifact(
                        &self.paths.stage_dir.join(artifact_name.as_str()),
                        bytes,
                        expected_hash.clone(),
                        FaultPoint::StageWrite,
                        FaultPoint::StageFsync,
                    )?;
                }
                (Some(None), ExpectedImageV1::Absent {}) => {}
                _ => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_plan",
                        "validated migration plan disagrees with its journal",
                    ));
                }
            }
        }

        let mut journal = plan.journal;
        self.write_journal(&journal, FaultPoint::PreparedJournalWrite)?;
        self.install_immutable_assets(&journal)?;
        let checkout_locks = self.acquire_checkout_action_locks(&journal)?;
        self.prevalidate_monotonic_checkout_actions_locked(&journal, &checkout_locks)?;
        self.verify_nonaction_checkout_bindings_locked(
            &planned_attachments,
            &journal,
            &checkout_locks,
        )?;
        self.execute_monotonic_checkout_actions_locked(&journal, &checkout_locks)?;
        for participant in &journal.participants {
            self.io.checkpoint(FaultPoint::ParticipantInstall)?;
            self.install_new_image(participant)?;
            self.io.checkpoint(FaultPoint::ParticipantInstall)?;
        }
        self.io.checkpoint(FaultPoint::CompletePlanVerify)?;
        self.verify_immutable_assets(&journal)?;
        self.verify_expected_pair(&journal, ExpectedSide::New)?;
        self.verify_migration_new_side_cross_roles(&journal)?;
        self.verify_journal_pair_invariants(&journal, ExpectedSide::New)?;
        self.io.checkpoint(FaultPoint::CompletePlanVerify)?;

        journal.state = TransactionStateV1::Committed;
        journal.outcome = Some(TransactionOutcomeV1::Committed);
        journal.committed_at = Some(unix_timestamp()?);
        journal.validate()?;
        self.preserve_committed_migration_journal(&mut journal)?;
        self.write_journal(&journal, FaultPoint::CommittedJournalWrite)?;
        let state = self.verify_current_migration_state(&journal)?;
        Ok(ProjectCatalogCommit {
            epoch: state.epoch,
            catalog_sha256: state.catalog_sha256.to_string(),
            attachments_sha256: state.attachments_sha256.to_string(),
        })
    }

    fn write_artifact(
        &self,
        path: &Path,
        bytes: &[u8],
        expected_hash: Sha256Hex,
        write_point: FaultPoint,
        fsync_point: FaultPoint,
    ) -> ProjectCatalogStoreResult<()> {
        if let Some(existing) = self.io.read_regular_nofollow(path, bytes.len())? {
            if sha256(&existing) != expected_hash {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_artifact_collision",
                    "content-addressed transaction artifact has unexpected bytes",
                ));
            }
            self.io.checkpoint(fsync_point)?;
            self.io.fsync_regular_nofollow(path)?;
            self.io.checkpoint(fsync_point)?;
            self.io.checkpoint(FaultPoint::DirectoryFsync)?;
            self.io
                .fsync_dir(path.parent().expect("derived artifact has parent"))?;
            self.io.checkpoint(FaultPoint::DirectoryFsync)?;
            return Ok(());
        }
        self.io.checkpoint(write_point)?;
        self.io.write_new_nofollow(path, bytes)?;
        self.io.checkpoint(write_point)?;
        self.io.checkpoint(fsync_point)?;
        self.io.fsync_regular_nofollow(path)?;
        self.io.checkpoint(fsync_point)?;
        self.io.checkpoint(FaultPoint::DirectoryFsync)?;
        self.io
            .fsync_dir(path.parent().expect("derived artifact has parent"))?;
        self.io.checkpoint(FaultPoint::DirectoryFsync)?;
        Ok(())
    }

    fn write_journal(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        point: FaultPoint,
    ) -> ProjectCatalogStoreResult<()> {
        let bytes = encode_bounded_json(journal, MAX_JOURNAL_BYTES, "transaction journal")?;
        self.io.checkpoint(point)?;
        self.io
            .atomic_replace_sync_nofollow(&self.paths.journal, &bytes)?;
        self.io.checkpoint(point)?;
        self.io.checkpoint(FaultPoint::DirectoryFsync)?;
        self.io.fsync_dir(
            self.paths
                .journal
                .parent()
                .expect("derived journal has parent"),
        )?;
        self.io.checkpoint(FaultPoint::DirectoryFsync)?;
        Ok(())
    }

    fn recover_locked(&self) -> ProjectCatalogStoreResult<()> {
        let Some(bytes) = self
            .io
            .read_regular_nofollow(&self.paths.journal, MAX_JOURNAL_BYTES)?
        else {
            return Ok(());
        };
        let mut journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "transaction journal")?;
        if journal.kind == TransactionKindV1::V1Migration
            && !matches!(self.registry, ParticipantRegistry::Migration(_))
        {
            // Runtime open over a terminal committed migration (phase-2
            // §4.1): the committed journal is deliberately retained, so the
            // regular owner verifies the registry-free pair subset here
            // (installed catalog/attachment images match the journal's new
            // hashes) and the strict open's origin/marker/journal binding
            // check covers the rest. Full participant and code-source
            // verification stays with the offline facade. Every
            // non-terminal migration journal still refuses: acting on one
            // requires the complete code-owned participant registry.
            if journal.state == TransactionStateV1::Committed
                && journal.outcome == Some(TransactionOutcomeV1::Committed)
            {
                journal.validate()?;
                let mut catalog_bytes = None;
                for role in [ParticipantRoleV1::Catalog, ParticipantRoleV1::Attachments] {
                    let participant = journal
                        .participants
                        .iter()
                        .find(|participant| participant.role == role)
                        .ok_or_else(|| {
                            ProjectCatalogStoreError::new(
                                "error.project_catalog_invalid_journal",
                                "committed migration journal lacks a pair participant",
                            )
                        })?;
                    let target = match role {
                        ParticipantRoleV1::Catalog => &self.paths.catalog,
                        _ => &self.paths.attachments,
                    };
                    let observed = self
                        .io
                        .read_regular_nofollow(target, participant.role.max_bytes())?;
                    if observed.as_deref().map(sha256).as_ref() != participant.new.sha256() {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_install_verification",
                            "installed pair does not match the committed migration journal",
                        ));
                    }
                    if role == ParticipantRoleV1::Catalog {
                        catalog_bytes = observed;
                    }
                }
                // The terminal journal must bind to the installed catalog:
                // a migration journal over a fresh-origin catalog is
                // incoherent state, not an openable root. The strict open's
                // origin/marker verification then closes the marker chain.
                let installed = catalog_bytes.ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "committed migration journal has no installed catalog",
                    )
                })?;
                let catalog = decode_catalog_snapshot(&installed).map_err(|error| {
                    ProjectCatalogStoreError::new(error.code(), error.to_string())
                })?;
                if catalog.origin
                    != (CatalogOriginV2::MigratedV1 {
                        transaction_id: journal.transaction_id.clone(),
                    })
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_migration_incomplete",
                        "committed migration journal does not bind to the catalog origin",
                    ));
                }
                return Ok(());
            }
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "migration recovery requires the complete code-owned participant registry",
            ));
        }
        journal.validate()?;
        match (journal.state, journal.outcome) {
            (TransactionStateV1::Committed, Some(TransactionOutcomeV1::Committed)) => {
                self.install_immutable_assets(&journal)?;
                self.verify_immutable_assets(&journal)?;
                if journal.kind == TransactionKindV1::V1Migration {
                    self.verify_current_migration_state(&journal).map(|_| ())
                } else {
                    self.verify_expected_pair(&journal, ExpectedSide::New)?;
                    self.verify_journal_pair_invariants(&journal, ExpectedSide::New)
                }
            }
            (TransactionStateV1::Committed, Some(TransactionOutcomeV1::RolledBack)) => {
                self.verify_expected_pair(&journal, ExpectedSide::Old)?;
                self.verify_journal_pair_invariants(&journal, ExpectedSide::Old)
            }
            (TransactionStateV1::Prepared, None) => {
                let rollback_available =
                    self.classify_recovery(&journal, true)? == RecoveryDecision::Rollback;
                let mut forward_available =
                    self.classify_recovery(&journal, false)? == RecoveryDecision::Forward;
                if forward_available && journal.kind == TransactionKindV1::V1Migration {
                    forward_available =
                        match self.verify_pinned_immutable_assets_for_recovery(&journal) {
                            Ok(()) => true,
                            Err(error)
                                if error.code() == "error.project_catalog_recovery_incomplete" =>
                            {
                                false
                            }
                            Err(error) => return Err(error),
                        };
                }
                let mut checkout_locks = Vec::new();
                if forward_available {
                    match self.acquire_checkout_action_locks(&journal) {
                        Ok(locks) => checkout_locks = locks,
                        Err(_) => forward_available = false,
                    }
                }
                if forward_available && journal.kind == TransactionKindV1::V1Migration {
                    forward_available = self.migration_forward_sources_available(&journal)?
                        && self.migration_forward_bindings_available(&journal, &checkout_locks)?;
                }
                let checkout_state = if forward_available {
                    self.classify_checkout_action_recovery(&journal, &checkout_locks)
                        .ok()
                } else {
                    None
                };
                forward_available &=
                    checkout_state == Some(CheckoutActionRecoveryState::Compatible);
                if forward_available {
                    if self
                        .execute_monotonic_checkout_actions_locked(&journal, &checkout_locks)
                        .is_err()
                    {
                        forward_available = false;
                    }
                }
                let decision = if forward_available {
                    RecoveryDecision::Forward
                } else if rollback_available {
                    RecoveryDecision::Rollback
                } else {
                    RecoveryDecision::Incomplete
                };
                match decision {
                    RecoveryDecision::Forward => {
                        self.install_immutable_assets(&journal)?;
                        self.verify_immutable_assets(&journal)?;
                        for participant in &journal.participants {
                            self.io.checkpoint(FaultPoint::RecoveryParticipantInstall)?;
                            self.install_new_image(participant)?;
                            self.io.checkpoint(FaultPoint::RecoveryParticipantInstall)?;
                        }
                        self.verify_expected_pair(&journal, ExpectedSide::New)?;
                        self.verify_migration_new_side_cross_roles(&journal)?;
                        self.verify_journal_pair_invariants(&journal, ExpectedSide::New)?;
                        let current_state_error = if journal.kind == TransactionKindV1::V1Migration
                        {
                            self.verify_current_migration_state(&journal).err()
                        } else {
                            None
                        };
                        if let Some(error) = current_state_error {
                            if !rollback_available {
                                return Err(error);
                            }
                            for participant in &journal.participants {
                                self.io.checkpoint(FaultPoint::RecoveryParticipantRestore)?;
                                self.restore_old_image(participant)?;
                                self.io.checkpoint(FaultPoint::RecoveryParticipantRestore)?;
                            }
                            self.verify_expected_pair(&journal, ExpectedSide::Old)?;
                            self.verify_journal_pair_invariants(&journal, ExpectedSide::Old)?;
                            journal.state = TransactionStateV1::Committed;
                            journal.outcome = Some(TransactionOutcomeV1::RolledBack);
                        } else {
                            journal.state = TransactionStateV1::Committed;
                            journal.outcome = Some(TransactionOutcomeV1::Committed);
                        }
                    }
                    RecoveryDecision::Rollback => {
                        for participant in &journal.participants {
                            self.io.checkpoint(FaultPoint::RecoveryParticipantRestore)?;
                            self.restore_old_image(participant)?;
                            self.io.checkpoint(FaultPoint::RecoveryParticipantRestore)?;
                        }
                        self.verify_expected_pair(&journal, ExpectedSide::Old)?;
                        self.verify_journal_pair_invariants(&journal, ExpectedSide::Old)?;
                        journal.state = TransactionStateV1::Committed;
                        journal.outcome = Some(TransactionOutcomeV1::RolledBack);
                    }
                    RecoveryDecision::Incomplete => {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_recovery_incomplete",
                            "neither complete forward recovery nor complete rollback is possible",
                        ));
                    }
                }
                if journal.kind == TransactionKindV1::V1Migration
                    && journal.outcome == Some(TransactionOutcomeV1::RolledBack)
                {
                    self.remove_provisional_migration_receipt_for_rollback(&journal)?;
                }
                journal.committed_at = Some(unix_timestamp()?);
                journal.validate()?;
                if journal.kind == TransactionKindV1::V1Migration
                    && journal.outcome == Some(TransactionOutcomeV1::Committed)
                {
                    self.preserve_committed_migration_journal(&mut journal)?;
                }
                self.write_journal(&journal, FaultPoint::CommittedJournalWrite)?;
                Ok(())
            }
            _ => Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_journal",
                "journal state and outcome disagree",
            )),
        }
    }

    fn migration_forward_bindings_available(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        checkout_locks: &[CatalogDirectoryLockGuard],
    ) -> ProjectCatalogStoreResult<bool> {
        let Some(attachment_participant) = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Attachments)
        else {
            return Ok(false);
        };
        let Some(attachment_bytes) = self.recovery_new_image_bytes(attachment_participant)? else {
            return Ok(false);
        };
        let Ok(attachments) = decode_attachment_snapshot(&attachment_bytes) else {
            return Ok(false);
        };
        Ok(self
            .verify_nonaction_checkout_bindings_locked(&attachments, journal, checkout_locks)
            .is_ok())
    }

    fn migration_forward_sources_available(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<bool> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Ok(false);
        };
        let publisher = self.io.read_regular_nofollow(
            &registry.legacy_publisher_ref_source,
            MAX_LEGACY_PUBLISHER_REF_SOURCE_BYTES,
        )?;
        let publisher_matches = match (journal.publisher_ref_source.as_ref(), publisher.as_deref())
        {
            (Some(MigrationPublisherSourceEvidenceV1::Missing { .. }), None) => true,
            (
                Some(MigrationPublisherSourceEvidenceV1::Present { sha256: expected }),
                Some(bytes),
            ) => sha256(bytes) == *expected,
            _ => false,
        };
        if !publisher_matches {
            return Ok(false);
        }
        let marker_participant = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::MigrationMarker);
        let Some(marker_participant) = marker_participant else {
            return Ok(false);
        };
        let Some(marker_bytes) = self.recovery_new_image_bytes(marker_participant)? else {
            return Ok(false);
        };
        let marker: ProjectCatalogMigrationMarkerV1 =
            decode_bounded_json(&marker_bytes, MAX_MARKER_BYTES, "migration marker")?;
        if verify_migration_marker_journal_binding(&marker, &marker_bytes, journal).is_err() {
            return Ok(false);
        }
        let catalog_participant = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Catalog);
        let Some(catalog_participant) = catalog_participant else {
            return Ok(false);
        };
        let Some(catalog_bytes) = self.recovery_new_image_bytes(catalog_participant)? else {
            return Ok(false);
        };
        let catalog = decode_catalog_snapshot(&catalog_bytes).map_err(contract_error)?;
        let mut collision_post_images = Vec::new();
        for participant in &journal.participants {
            let ParticipantRoleV1::CollisionRetirement { .. } = &participant.role else {
                continue;
            };
            let Some(bytes) = self.recovery_new_image_bytes(participant)? else {
                return Ok(false);
            };
            collision_post_images.push((participant.role.clone(), bytes));
        }
        let inventory_scopes = migration_inventory_scopes(
            &catalog,
            collision_post_images
                .iter()
                .map(|(role, bytes)| (role, Some(bytes.as_slice()))),
        )?;
        if self.all_participants_installed_new(journal)? {
            return Ok(true);
        }
        let inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &inventory_scopes,
        );
        Ok(inventory
            .is_ok_and(|inventory| inventory.canonical_sha256 == marker.inventory_sha256.as_str()))
    }

    fn recovery_new_image_bytes(
        &self,
        participant: &TransactionParticipantV1,
    ) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
        let ExpectedImageV1::Present {
            sha256: expected,
            artifact_name,
        } = &participant.new
        else {
            return Ok(None);
        };
        if let Some(bytes) = self.io.read_regular_nofollow(
            &self.paths.stage_dir.join(artifact_name.as_str()),
            participant.role.max_bytes(),
        )? && sha256(&bytes) == *expected
        {
            return Ok(Some(bytes));
        }
        let target = self.target_for_role(&participant.role)?;
        Ok(self
            .io
            .read_regular_nofollow(&target, participant.role.max_bytes())?
            .filter(|bytes| sha256(bytes) == *expected))
    }

    fn all_participants_installed_new(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<bool> {
        for participant in &journal.participants {
            let target = self.target_for_role(&participant.role)?;
            let actual = self
                .io
                .read_regular_nofollow(&target, participant.role.max_bytes())?;
            let matches = match &participant.new {
                ExpectedImageV1::Absent {} => actual.is_none(),
                ExpectedImageV1::Present {
                    sha256: expected, ..
                } => actual.is_some_and(|bytes| sha256(&bytes) == *expected),
            };
            if !matches {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn classify_recovery(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        prefer_rollback: bool,
    ) -> ProjectCatalogStoreResult<RecoveryDecision> {
        let mut forward = true;
        let mut rollback = true;
        for participant in &journal.participants {
            let target = self.target_for_role(&participant.role)?;
            let observed = self
                .io
                .read_regular_nofollow(&target, participant.role.max_bytes())?
                .map(|bytes| sha256(&bytes));
            let old_hash = participant.old.sha256();
            let new_hash = participant.new.sha256();
            let target_is_new = observed.as_ref() == new_hash;
            let forward_available = match &participant.new {
                ExpectedImageV1::Absent {} => observed.is_none() || observed.as_ref() == old_hash,
                image @ ExpectedImageV1::Present { .. } => {
                    target_is_new
                        || self.artifact_available(
                            &self.paths.stage_dir,
                            image,
                            participant.role.max_bytes(),
                        )?
                }
            };
            forward &= forward_available;

            let target_is_old = match old_hash {
                Some(hash) => observed.as_ref() == Some(hash),
                None => observed.is_none(),
            };
            let backup_available = match &participant.old {
                ExpectedImageV1::Absent {} => observed.is_none() || observed.as_ref() == new_hash,
                image @ ExpectedImageV1::Present { .. } => self.artifact_available(
                    &self.paths.backup_dir,
                    image,
                    participant.role.max_bytes(),
                )?,
            };
            rollback &= target_is_old || backup_available;

            let explained = observed.is_none()
                || observed.as_ref() == old_hash
                || observed.as_ref() == new_hash;
            if !explained {
                return Ok(RecoveryDecision::Incomplete);
            }
        }
        Ok(if prefer_rollback {
            if rollback {
                RecoveryDecision::Rollback
            } else {
                RecoveryDecision::Incomplete
            }
        } else if forward {
            RecoveryDecision::Forward
        } else if rollback {
            RecoveryDecision::Rollback
        } else {
            RecoveryDecision::Incomplete
        })
    }

    fn install_new_image(
        &self,
        participant: &TransactionParticipantV1,
    ) -> ProjectCatalogStoreResult<()> {
        let target = self.target_for_role(&participant.role)?;
        let max_bytes = participant.role.max_bytes();
        if let Some(parent) = target.parent() {
            self.io.create_private_dir_nofollow(parent)?;
        }
        match &participant.new {
            ExpectedImageV1::Absent {} => {
                let Some(bytes) = self.io.read_regular_nofollow(&target, max_bytes)? else {
                    return Ok(());
                };
                let old_hash = participant.old.sha256().ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_journal",
                        "deletion participant has no old image hash",
                    )
                })?;
                if sha256(&bytes) != *old_hash {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "deletion participant contains unexplained target bytes",
                    ));
                }
                self.io.remove_regular_exact(&target, old_hash, max_bytes)?;
                self.fsync_dir_checkpointed(
                    target
                        .parent()
                        .expect("code-owned participant target has parent"),
                )
            }
            ExpectedImageV1::Present {
                sha256: expected_hash,
                artifact_name,
            } => {
                if self
                    .io
                    .read_regular_nofollow(&target, max_bytes)?
                    .is_some_and(|bytes| sha256(&bytes) == *expected_hash)
                {
                    return Ok(());
                }
                let stage = self.paths.stage_dir.join(artifact_name.as_str());
                self.io
                    .replace_from_stage_nofollow(&stage, &target, expected_hash, max_bytes)?;
                self.fsync_dir_checkpointed(
                    target
                        .parent()
                        .expect("code-owned participant target has parent"),
                )
            }
        }
    }

    fn restore_old_image(
        &self,
        participant: &TransactionParticipantV1,
    ) -> ProjectCatalogStoreResult<()> {
        let target = self.target_for_role(&participant.role)?;
        let max_bytes = participant.role.max_bytes();
        if let Some(parent) = target.parent() {
            self.io.create_private_dir_nofollow(parent)?;
        }
        match &participant.old {
            ExpectedImageV1::Absent {} => {
                if let Some(bytes) = self.io.read_regular_nofollow(&target, max_bytes)? {
                    let new_hash = participant.new.sha256().ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_invalid_journal",
                            "regular participant lacks a new image hash",
                        )
                    })?;
                    if sha256(&bytes) != *new_hash {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_recovery_incomplete",
                            "rollback refused to remove unexplained target bytes",
                        ));
                    }
                    self.io.checkpoint(FaultPoint::RecoveryParticipantDelete)?;
                    self.io.remove_regular_exact(&target, new_hash, max_bytes)?;
                    self.io.checkpoint(FaultPoint::RecoveryParticipantDelete)?;
                    self.fsync_dir_checkpointed(
                        target
                            .parent()
                            .expect("code-owned participant target has parent"),
                    )?;
                }
                Ok(())
            }
            ExpectedImageV1::Present {
                sha256: expected_hash,
                artifact_name,
            } => {
                if self
                    .io
                    .read_regular_nofollow(&target, max_bytes)?
                    .is_some_and(|bytes| sha256(&bytes) == *expected_hash)
                {
                    return Ok(());
                }
                let backup = self.paths.backup_dir.join(artifact_name.as_str());
                self.io
                    .replace_from_stage_nofollow(&backup, &target, expected_hash, max_bytes)?;
                self.fsync_dir_checkpointed(
                    target
                        .parent()
                        .expect("code-owned participant target has parent"),
                )
            }
        }
    }

    fn verify_expected_pair(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        side: ExpectedSide,
    ) -> ProjectCatalogStoreResult<()> {
        for participant in &journal.participants {
            let expected = match side {
                ExpectedSide::Old => &participant.old,
                ExpectedSide::New => &participant.new,
            };
            let target = self.target_for_role(&participant.role)?;
            let observed = self
                .io
                .read_regular_nofollow(&target, participant.role.max_bytes())?
                .map(|bytes| sha256(&bytes));
            if observed.as_ref() != expected.sha256() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_install_verification",
                    "installed participant hash does not match journal",
                ));
            }
        }
        Ok(())
    }

    fn verify_migration_new_side_cross_roles(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        if journal.kind != TransactionKindV1::V1Migration {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "migration cross-role verification requires the complete registry",
            ));
        };
        let mut post_images = std::collections::BTreeMap::new();
        for participant in &journal.participants {
            let target = self.target_for_role(&participant.role)?;
            let bytes = self
                .io
                .read_regular_nofollow(&target, participant.role.max_bytes())?;
            if bytes.as_ref().map(|value| sha256(value)).as_ref() != participant.new.sha256() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "migration participant changed before cross-role verification",
                ));
            }
            post_images.insert(participant.role.clone(), bytes);
        }
        let catalog = decode_catalog_snapshot(
            post_images
                .get(&ParticipantRoleV1::Catalog)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "migration catalog is missing during cross-role verification",
                    )
                })?,
        )
        .map_err(contract_error)?;
        let attachments = decode_attachment_snapshot(
            post_images
                .get(&ParticipantRoleV1::Attachments)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "migration attachments are missing during cross-role verification",
                    )
                })?,
        )
        .map_err(contract_error)?;
        validate_checkout_bindings(
            registry,
            &attachments,
            &journal.monotonic_checkout_identity_actions,
        )
        .map_err(|error| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                error.to_string(),
            )
        })?;
        let marker: ProjectCatalogMigrationMarkerV1 = decode_bounded_json(
            post_images
                .get(&ParticipantRoleV1::MigrationMarker)
                .and_then(Option::as_deref)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "migration marker is missing during cross-role verification",
                    )
                })?,
            MAX_MARKER_BYTES,
            "migration marker",
        )?;
        marker.validate()?;
        let immutable_assets = journal
            .immutable_assets
            .iter()
            .map(|asset| MigrationImmutableAssetEvidenceV1 {
                role: asset.role.clone(),
                mode: asset.mode,
                sha256: asset.sha256.clone(),
                validated_name: asset.validated_name.clone(),
            })
            .collect::<Vec<_>>();
        let mut immutable_asset_bytes = std::collections::BTreeMap::new();
        for asset in &journal.immutable_assets {
            if !matches!(
                &asset.role,
                ImmutableAssetRoleV1::AcceptedPublicationGeneration { .. }
                    | ImmutableAssetRoleV1::LegacyPublisherRefBackup
            ) {
                continue;
            }
            let target = registry.immutable_target(&asset.role, &asset.validated_name);
            let bytes = self
                .io
                .read_regular_nofollow(&target, asset.role.max_bytes())?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "accepted publication generation is missing during verification",
                    )
                })?;
            immutable_asset_bytes.insert(asset.role.clone(), bytes);
        }
        match journal.publisher_ref_source.as_ref() {
            Some(MigrationPublisherSourceEvidenceV1::Missing { .. })
                if !immutable_asset_bytes
                    .contains_key(&ImmutableAssetRoleV1::LegacyPublisherRefBackup) => {}
            Some(MigrationPublisherSourceEvidenceV1::Present { sha256: expected }) => {
                let publisher_source = immutable_asset_bytes
                    .get(&ImmutableAssetRoleV1::LegacyPublisherRefBackup)
                    .ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_recovery_incomplete",
                            "present publisher source backup is missing during verification",
                        )
                    })?;
                if sha256(publisher_source) != *expected {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "present publisher source backup hash disagrees during verification",
                    ));
                }
                validate_publisher_source_binding(
                    publisher_source,
                    &journal.publisher_pins,
                    "retained migration source",
                )?;
            }
            _ => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "publisher source presence evidence is inconsistent",
                ));
            }
        }
        validate_new_side_cross_roles(
            &catalog,
            &attachments,
            &post_images,
            &immutable_assets,
            &immutable_asset_bytes,
            &journal.publisher_pins,
            &journal.publisher_dispositions,
            "error.project_catalog_recovery_incomplete",
        )?;
        self.verify_journaled_code_source_transition(journal, &post_images, &marker)
    }

    fn verify_journaled_code_source_transition(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        post_images: &BTreeMap<ParticipantRoleV1, Option<Vec<u8>>>,
        marker: &ProjectCatalogMigrationMarkerV1,
    ) -> ProjectCatalogStoreResult<()> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "journaled source verification requires the complete registry",
            ));
        };
        let fail = |detail: &str| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                format!("journaled code-source transition is invalid: {detail}"),
            )
        };
        let participant_for = |role: &ParticipantRoleV1| {
            journal
                .participants
                .iter()
                .find(|participant| &participant.role == role)
        };
        let read_old = |participant: &TransactionParticipantV1| {
            let ExpectedImageV1::Present {
                sha256: expected,
                artifact_name,
            } = &participant.old
            else {
                return Ok(None);
            };
            let bytes = self
                .io
                .read_regular_nofollow(
                    &self.paths.backup_dir.join(artifact_name.as_str()),
                    participant.role.max_bytes(),
                )?
                .ok_or_else(|| fail("old participant backup is missing"))?;
            if sha256(&bytes) != *expected {
                return Err(fail("old participant backup hash disagrees"));
            }
            Ok(Some(bytes))
        };
        let verify_retirement_preimage = |role: &ParticipantRoleV1,
                                          retirement: &CollisionRetirementLifecycleV1|
         -> ProjectCatalogStoreResult<bool> {
            let participant = participant_for(role)
                .ok_or_else(|| fail("collision retirement participant is missing"))?;
            match &participant.old {
                ExpectedImageV1::Absent {} => Ok(true),
                ExpectedImageV1::Present { .. } => {
                    let previous_bytes = read_old(participant)?
                        .ok_or_else(|| fail("collision retirement old bytes are absent"))?;
                    let previous =
                        decode_collision_retirement_pending_for_migration(&previous_bytes)
                            .map_err(|error| fail(&error.to_string()))?;
                    if &previous != retirement {
                        return Err(fail(
                            "migration rewrites an existing collision retirement lifecycle",
                        ));
                    }
                    Ok(false)
                }
            }
        };
        let effective_participant = participant_for(&ParticipantRoleV1::EffectiveSourceManifest)
            .ok_or_else(|| fail("effective source participant is missing"))?;
        let old_effective = read_old(effective_participant)?
            .as_deref()
            .map(decode_migration_effective_source_manifest_v1)
            .transpose()
            .map_err(|error| fail(&error.to_string()))?;
        let new_effective = post_images
            .get(&ParticipantRoleV1::EffectiveSourceManifest)
            .and_then(Option::as_deref)
            .ok_or_else(|| fail("effective source post-image is missing"))
            .and_then(|bytes| {
                decode_migration_effective_source_manifest_v1(bytes)
                    .map_err(|error| fail(&error.to_string()))
            })?;
        let old_selections = old_effective
            .as_ref()
            .map(|effective| {
                effective
                    .selections
                    .iter()
                    .map(|selection| (selection.project_id.clone(), selection))
                    .collect::<BTreeMap<_, _>>()
            })
            .unwrap_or_default();
        let new_selections = new_effective
            .selections
            .iter()
            .map(|selection| (selection.project_id.clone(), selection))
            .collect::<BTreeMap<_, _>>();
        let mut accounted_activations = BTreeSet::new();
        let mut accounted_retirements = BTreeSet::new();
        let mut accounted_old_selections = BTreeSet::new();
        let mut accounted_new_selections = BTreeSet::new();

        for stored_participant in &journal.participants {
            let ParticipantRoleV1::StoredGenerationMetadata {
                project_id,
                published_scope,
                generation_id,
            } = &stored_participant.role
            else {
                continue;
            };
            let old_stored_bytes = read_old(stored_participant)?
                .ok_or_else(|| fail("stored generation lacks exact old bytes"))?;
            let old_stored = decode_stored_generation_v1_for_migration(&old_stored_bytes)
                .map_err(|error| fail(&error.to_string()))?;
            if old_stored.generation_id != generation_id.as_str()
                || &old_stored.descriptor.scope != published_scope
            {
                return Err(fail("stored generation old bytes disagree with role"));
            }
            let expected_stored =
                bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
                    old_stored.clone(),
                    published_scope.clone(),
                )
                .map_err(|error| fail(&error.to_string()))?;
            let manifest_asset = journal
                .immutable_assets
                .iter()
                .find(|asset| {
                    asset.role
                        == (ImmutableAssetRoleV1::CollectedGenerationManifest {
                            published_scope: published_scope.clone(),
                            generation_id: generation_id.clone(),
                        })
                })
                .ok_or_else(|| fail("stored generation lacks pinned manifest evidence"))?;
            let manifest_bytes = self
                .io
                .read_regular_nofollow(
                    &registry
                        .immutable_target(&manifest_asset.role, &manifest_asset.validated_name),
                    MAX_CODE_SOURCE_COLLECTED_MANIFEST_BYTES,
                )?
                .ok_or_else(|| fail("pinned generation manifest is missing"))?;
            if sha256(&manifest_bytes) != manifest_asset.sha256 {
                return Err(fail("pinned generation manifest hash disagrees"));
            }
            verify_generation_manifest_for_migration(
                &manifest_bytes,
                &old_stored.descriptor,
                &old_stored.producer_id,
                generation_id.as_str(),
                &registry.code_source_limits,
            )
            .map_err(|error| fail(&error.to_string()))?;
            let selection_matches =
                |selection: &&bbox_code_source_store::MigrationEffectiveSourceSelectionV1,
                 selector: &str| {
                    &selection.published_scope == published_scope
                        && selection.generation_id == generation_id.as_str()
                        && selection.selector == selector
                };
            let old_selection = old_selections.get(project_id);
            let new_selection = new_selections.get(project_id);
            let activation_role = ParticipantRoleV1::Activation {
                project_id: project_id.clone(),
            };
            let activation_participant = participant_for(&activation_role);
            match &stored_participant.new {
                ExpectedImageV1::Present { .. } => {
                    let new_stored_bytes = post_images
                        .get(&stored_participant.role)
                        .and_then(Option::as_deref)
                        .ok_or_else(|| fail("surviving stored generation is absent"))?;
                    let new_stored = decode_stored_generation_v2_for_migration(new_stored_bytes)
                        .map_err(|error| fail(&error.to_string()))?;
                    if new_stored != expected_stored {
                        return Err(fail("surviving stored generation rewrites old evidence"));
                    }
                    let matching_activation = activation_participant
                        .map(|activation_participant| {
                            let bytes = read_old(activation_participant)?
                                .ok_or_else(|| fail("activation lacks exact old bytes"))?;
                            let activation = decode_activation_v1_for_migration(&bytes)
                                .map_err(|error| fail(&error.to_string()))?;
                            Ok::<_, ProjectCatalogStoreError>((activation_participant, activation))
                        })
                        .transpose()?
                        .filter(|(_, activation)| {
                            activation.generation_id == generation_id.as_str()
                        });
                    match matching_activation {
                        Some((activation_participant, old_activation))
                            if matches!(
                                &activation_participant.new,
                                ExpectedImageV1::Present { .. }
                            ) =>
                        {
                            let expected_activation =
                                bbox_code_source_store::ActivationRecordV2::from_v1_for_migration(
                                    old_activation,
                                    &expected_stored,
                                )
                                .map_err(|error| fail(&error.to_string()))?;
                            let new_activation_bytes = post_images
                                .get(&activation_role)
                                .and_then(Option::as_deref)
                                .ok_or_else(|| fail("active generation is absent"))?;
                            let new_activation =
                                decode_activation_v2_for_migration(new_activation_bytes)
                                    .map_err(|error| fail(&error.to_string()))?;
                            if new_activation != expected_activation
                                || old_effective.is_some()
                                    && !old_selection.is_some_and(|selection| {
                                        selection_matches(selection, &expected_activation.selector)
                                    })
                                || !new_selection.is_some_and(|selection| {
                                    selection_matches(selection, &expected_activation.selector)
                                })
                            {
                                return Err(fail(
                                    "active generation transition rewrites source evidence",
                                ));
                            }
                            accounted_activations.insert(activation_role);
                            if old_effective.is_some() {
                                accounted_old_selections.insert(project_id.clone());
                            }
                            accounted_new_selections.insert(project_id.clone());
                        }
                        Some((activation_participant, old_activation))
                            if matches!(
                                &activation_participant.new,
                                ExpectedImageV1::Absent {}
                            ) =>
                        {
                            if old_effective.is_some()
                                && !old_selection.is_some_and(|selection| {
                                    selection_matches(selection, &old_activation.selector)
                                })
                                || new_selection.is_some()
                            {
                                return Err(fail(
                                    "quarantined generation remains active or selected",
                                ));
                            }
                            let retirement_role = ParticipantRoleV1::CollisionRetirement {
                                project_id: project_id.clone(),
                            };
                            let retirement_bytes = post_images
                                .get(&retirement_role)
                                .and_then(Option::as_deref)
                                .ok_or_else(|| {
                                    fail("quarantined generation lacks retirement record")
                                })?;
                            let retirement =
                                decode_collision_retirement_pending_for_migration(retirement_bytes)
                                    .map_err(|error| fail(&error.to_string()))?;
                            let lifecycle_was_new =
                                verify_retirement_preimage(&retirement_role, &retirement)?;
                            let retirement_entry =
                                retirement.entries.get(generation_id.as_str()).ok_or_else(
                                    || fail("quarantined generation lacks lifecycle entry"),
                                )?;
                            if &retirement.project_id != project_id
                                || &retirement_entry.former_scope != published_scope
                                || retirement_entry.exact_selector()
                                    != Some(old_activation.selector.as_str())
                                || retirement_entry.snapshot_id != old_activation.snapshot_id
                                || retirement_entry.manifest_sha256
                                    != old_stored.descriptor.manifest_sha256
                                || (lifecycle_was_new
                                    && (retirement_entry.inventory_hash
                                        != marker.inventory_sha256.as_str()
                                        || retirement_entry.plan_hash != marker.plan_hash.as_str()))
                            {
                                return Err(fail(
                                    "collision retirement rewrites exact old source evidence",
                                ));
                            }
                            accounted_activations.insert(activation_role);
                            accounted_retirements.insert(retirement_role);
                            if old_effective.is_some() {
                                accounted_old_selections.insert(project_id.clone());
                            }
                        }
                        None => {
                            let selected_here = old_selection.is_some_and(|selection| {
                                selection.generation_id == generation_id.as_str()
                            }) || new_selection.is_some_and(|selection| {
                                selection.generation_id == generation_id.as_str()
                            });
                            if selected_here {
                                return Err(fail("retained generation is unexpectedly selected"));
                            }
                            let retirement_role = ParticipantRoleV1::CollisionRetirement {
                                project_id: project_id.clone(),
                            };
                            if let Some(Some(retirement_bytes)) = post_images.get(&retirement_role)
                            {
                                let retirement = decode_collision_retirement_pending_for_migration(
                                    retirement_bytes,
                                )
                                .map_err(|error| fail(&error.to_string()))?;
                                let lifecycle_was_new =
                                    verify_retirement_preimage(&retirement_role, &retirement)?;
                                if let Some(retirement_entry) =
                                    retirement.entries.get(generation_id.as_str())
                                {
                                    if &retirement.project_id != project_id
                                        || &retirement_entry.former_scope != published_scope
                                        || retirement_entry.selector_evidence
                                            != CollisionRetirementSelectorEvidenceV1::NoDurableSelector
                                        || retirement_entry.manifest_sha256
                                            != old_stored.descriptor.manifest_sha256
                                        || (lifecycle_was_new
                                            && (retirement_entry.inventory_hash
                                                != marker.inventory_sha256.as_str()
                                                || retirement_entry.plan_hash
                                                    != marker.plan_hash.as_str()))
                                    {
                                        return Err(fail(
                                            "retained collision rewrites owner or immutable evidence",
                                        ));
                                    }
                                    accounted_retirements.insert(retirement_role);
                                }
                            }
                        }
                        Some(_) => return Err(fail("activation transition shape is invalid")),
                    }
                }
                ExpectedImageV1::Absent {} => {
                    let matches_activation = activation_participant
                        .map(|activation_participant| {
                            let bytes = read_old(activation_participant)?
                                .ok_or_else(|| fail("activation lacks exact old bytes"))?;
                            let activation = decode_activation_v1_for_migration(&bytes)
                                .map_err(|error| fail(&error.to_string()))?;
                            Ok::<_, ProjectCatalogStoreError>(
                                activation.generation_id == generation_id.as_str(),
                            )
                        })
                        .transpose()?
                        .unwrap_or(false);
                    if matches_activation
                        || old_selection.is_some_and(|selection| {
                            selection.generation_id == generation_id.as_str()
                        })
                        || new_selection.is_some_and(|selection| {
                            selection.generation_id == generation_id.as_str()
                        })
                    {
                        return Err(fail(
                            "migration deletes active, selected, or quarantined generation metadata",
                        ));
                    }
                }
            }
        }
        if accounted_old_selections != old_selections.keys().cloned().collect::<BTreeSet<_>>()
            || accounted_new_selections != new_selections.keys().cloned().collect::<BTreeSet<_>>()
        {
            return Err(fail(
                "effective source manifest contains an unaccounted selection",
            ));
        }
        for participant in &journal.participants {
            match &participant.role {
                ParticipantRoleV1::Activation { .. }
                    if !accounted_activations.contains(&participant.role) =>
                {
                    return Err(fail("activation lacks stored generation evidence"));
                }
                ParticipantRoleV1::CollisionRetirement { .. }
                    if !accounted_retirements.contains(&participant.role) =>
                {
                    return Err(fail("retirement lacks deleted generation evidence"));
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn verify_journal_pair_invariants(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        side: ExpectedSide,
    ) -> ProjectCatalogStoreResult<()> {
        let epoch = match side {
            ExpectedSide::Old => journal.old_epoch,
            ExpectedSide::New => journal.new_epoch,
        };
        if epoch == 0 {
            let catalog = self
                .io
                .read_regular_nofollow(&self.paths.catalog, MAX_LEGACY_PROJECT_STORE_BYTES)?;
            let attachments = self
                .io
                .read_regular_nofollow(&self.paths.attachments, MAX_PROJECT_CATALOG_BYTES)?;
            match journal.kind {
                TransactionKindV1::RegularPair if catalog.is_none() && attachments.is_none() => {
                    return Ok(());
                }
                TransactionKindV1::V1Migration if side == ExpectedSide::Old => {
                    let legacy_matches =
                        match (journal.legacy_project_source.as_ref(), catalog.as_deref()) {
                            (
                                Some(MigrationLegacyProjectSourceEvidenceV1::Missing { .. }),
                                None,
                            ) => true,
                            (
                                Some(MigrationLegacyProjectSourceEvidenceV1::Present {
                                    sha256: expected,
                                }),
                                Some(bytes),
                            ) => {
                                decode_legacy_project_store(bytes).is_ok()
                                    && sha256(bytes) == *expected
                            }
                            _ => false,
                        };
                    if attachments.is_none() && legacy_matches {
                        return Ok(());
                    }
                }
                _ => {}
            }
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_install_verification",
                "epoch-zero journal state does not match its transaction kind",
            ));
        }
        let installed = self.read_strict_pair_locked()?;
        if installed.epoch != epoch {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_install_verification",
                "installed pair epoch does not match journal",
            ));
        }
        Ok(())
    }

    fn artifact_available(
        &self,
        root: &Path,
        image: &ExpectedImageV1,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<bool> {
        let ExpectedImageV1::Present {
            sha256: expected_hash,
            artifact_name,
        } = image
        else {
            return Ok(false);
        };
        Ok(self
            .io
            .read_regular_nofollow(&root.join(artifact_name.as_str()), max_bytes)?
            .is_some_and(|bytes| sha256(&bytes) == *expected_hash))
    }

    fn verify_immutable_assets(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        if journal.immutable_assets.is_empty() {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "immutable migration assets require the complete registry",
            ));
        };
        for asset in &journal.immutable_assets {
            self.io.checkpoint(FaultPoint::ImmutableAssetVerify)?;
            let target = registry.immutable_target(&asset.role, &asset.validated_name);
            if target.file_name().and_then(|name| name.to_str())
                != Some(asset.validated_name.as_str())
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "journaled immutable target name is not code-derived",
                ));
            }
            let bytes = self
                .io
                .read_regular_nofollow(&target, asset.role.max_bytes())?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "journaled immutable migration asset is missing",
                    )
                })?;
            if sha256(&bytes) != asset.sha256 {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "journaled immutable migration asset has unexpected bytes",
                ));
            }
            self.io.checkpoint(FaultPoint::ImmutableAssetVerify)?;
        }
        Ok(())
    }

    fn verify_pinned_immutable_assets_for_recovery(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        if !journal
            .immutable_assets
            .iter()
            .any(|asset| asset.mode == ImmutableAssetModeV1::PinnedExisting)
        {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "pinned immutable recovery evidence requires the complete registry",
            ));
        };
        for asset in &journal.immutable_assets {
            if asset.mode != ImmutableAssetModeV1::PinnedExisting {
                continue;
            }
            let target = registry.immutable_target(&asset.role, &asset.validated_name);
            let bytes = self
                .io
                .read_regular_nofollow(&target, asset.role.max_bytes())?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "pinned immutable recovery asset is missing",
                    )
                })?;
            if sha256(&bytes) != asset.sha256 {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "pinned immutable recovery asset has unexpected bytes",
                ));
            }
        }
        Ok(())
    }

    fn install_immutable_assets(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        if journal.immutable_assets.is_empty() {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "immutable migration assets require the complete registry",
            ));
        };
        for asset in &journal.immutable_assets {
            let target = registry.immutable_target(&asset.role, &asset.validated_name);
            if target.file_name().and_then(|name| name.to_str())
                != Some(asset.validated_name.as_str())
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "journaled immutable target name is not code-derived",
                ));
            }
            let parent = target.parent().ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_unsafe_path",
                    "immutable migration target has no parent",
                )
            })?;
            let existing = self
                .io
                .read_regular_nofollow(&target, asset.role.max_bytes())?;
            if asset.mode == ImmutableAssetModeV1::PinnedExisting {
                let bytes = existing.ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "pinned immutable migration asset is missing",
                    )
                })?;
                if sha256(&bytes) != asset.sha256 {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "pinned immutable migration asset has unexpected bytes",
                    ));
                }
                continue;
            }
            self.io.create_private_dir_nofollow(parent)?;
            if let Some(existing) = existing {
                if sha256(&existing) != asset.sha256 {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_artifact_collision",
                        "immutable migration target has unexpected bytes",
                    ));
                }
                self.io.fsync_regular_nofollow(&target)?;
                self.fsync_dir_checkpointed(parent)?;
                continue;
            }

            let stage_name = asset.stage_name.as_ref().ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "installable immutable migration asset lacks its stage name",
                )
            })?;
            let stage = self.paths.stage_dir.join(stage_name.as_str());
            let bytes = self
                .io
                .read_regular_nofollow(&stage, asset.role.max_bytes())?
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "journaled immutable stage is missing",
                    )
                })?;
            if sha256(&bytes) != asset.sha256 {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "journaled immutable stage has unexpected bytes",
                ));
            }
            self.io.checkpoint(FaultPoint::ImmutableAssetWrite)?;
            match self.io.write_new_nofollow(&target, &bytes) {
                Ok(()) => {}
                Err(error) if error.code() == "error.project_catalog_already_exists" => {
                    let installed = self
                        .io
                        .read_regular_nofollow(&target, asset.role.max_bytes())?
                        .ok_or_else(|| {
                            ProjectCatalogStoreError::new(
                                "error.project_catalog_recovery_incomplete",
                                "immutable target disappeared after create contention",
                            )
                        })?;
                    if sha256(&installed) != asset.sha256 {
                        return Err(ProjectCatalogStoreError::new(
                            "error.project_catalog_artifact_collision",
                            "immutable target changed during no-replace installation",
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
            self.io.checkpoint(FaultPoint::ImmutableAssetWrite)?;
            self.io.checkpoint(FaultPoint::ImmutableAssetFsync)?;
            self.io.fsync_regular_nofollow(&target)?;
            self.io.checkpoint(FaultPoint::ImmutableAssetFsync)?;
            self.fsync_dir_checkpointed(parent)?;
        }
        Ok(())
    }

    fn fsync_dir_checkpointed(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
        self.io.checkpoint(FaultPoint::DirectoryFsync)?;
        self.io.fsync_dir(path)?;
        self.io.checkpoint(FaultPoint::DirectoryFsync)
    }

    fn prevalidate_monotonic_checkout_actions(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<()> {
        if journal.monotonic_checkout_identity_actions.is_empty() {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout identity actions require the complete migration registry",
            ));
        };
        for action in &journal.monotonic_checkout_identity_actions {
            let target = registry
                .checkout_identity_target(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration registry lacks a checkout identity target",
                    )
                })?;
            let parent = target.parent().ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_unsafe_path",
                    "checkout identity target has no parent",
                )
            })?;
            match self.io.read_regular_nofollow(&target, 128)? {
                None => {}
                Some(bytes)
                    if bytes.is_empty()
                        || checkout_marker_bytes_match(&bytes, &action.planned_id) => {}
                Some(_) => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "checkout identity marker disagrees with the journaled id",
                    ));
                }
            }
            let gitignore = parent.join(".gitignore");
            let _ = self.io.read_regular_nofollow(&gitignore, 64 * 1024)?;
        }
        Ok(())
    }

    fn classify_checkout_action_recovery(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        locks: &[CatalogDirectoryLockGuard],
    ) -> ProjectCatalogStoreResult<CheckoutActionRecoveryState> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            if journal.monotonic_checkout_identity_actions.is_empty() {
                return Ok(CheckoutActionRecoveryState::Compatible);
            }
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout identity actions require the complete migration registry",
            ));
        };
        let mut state = CheckoutActionRecoveryState::Compatible;
        for action in &journal.monotonic_checkout_identity_actions {
            let target = registry
                .checkout_identity_target(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration registry lacks a checkout identity target",
                    )
                })?;
            let lane = checkout_lock_for(&target, locks)?;
            match lane.read_regular("checkout-id", 128)? {
                None => {}
                Some(bytes)
                    if bytes.is_empty()
                        || checkout_marker_bytes_match(&bytes, &action.planned_id) => {}
                Some(bytes) if valid_checkout_identity_bytes(&bytes) => {
                    state = CheckoutActionRecoveryState::ConflictingValid;
                }
                Some(_) => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "checkout identity marker is malformed during migration recovery",
                    ));
                }
            }
            lane.ensure_still_current()?;
        }
        Ok(state)
    }

    fn prevalidate_monotonic_checkout_actions_locked(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        locks: &[CatalogDirectoryLockGuard],
    ) -> ProjectCatalogStoreResult<()> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            if journal.monotonic_checkout_identity_actions.is_empty() {
                return Ok(());
            }
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout identity actions require the complete migration registry",
            ));
        };
        for action in &journal.monotonic_checkout_identity_actions {
            let target = registry
                .checkout_identity_target(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration registry lacks a checkout identity target",
                    )
                })?;
            let lane = checkout_lock_for(&target, locks)?;
            match lane.read_regular("checkout-id", 128)? {
                None => {}
                Some(bytes)
                    if bytes.is_empty()
                        || checkout_marker_bytes_match(&bytes, &action.planned_id) => {}
                Some(_) => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "checkout identity marker disagrees with the journaled id",
                    ));
                }
            }
            let _ = lane.read_regular(".gitignore", 64 * 1024)?;
            lane.ensure_still_current()?;
        }
        Ok(())
    }

    fn acquire_checkout_action_locks(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
    ) -> ProjectCatalogStoreResult<Vec<CatalogDirectoryLockGuard>> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            if journal.monotonic_checkout_identity_actions.is_empty() {
                return Ok(Vec::new());
            }
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout identity actions require the complete migration registry",
            ));
        };
        let mut parents = registry
            .checkout_identity_markers
            .values()
            .map(|target| {
                target.parent().map(Path::to_path_buf).ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration registry lacks a checkout identity parent",
                    )
                })
            })
            .collect::<ProjectCatalogStoreResult<Vec<_>>>()?;
        parents.sort();
        parents.dedup();
        let mut guards = Vec::with_capacity(parents.len());
        for parent in parents {
            self.io.create_private_dir_nofollow(&parent)?;
            guards.push(self.io.acquire_directory_lock_nofollow(&parent)?);
        }
        Ok(guards)
    }

    fn verify_nonaction_checkout_bindings_locked(
        &self,
        attachments: &AttachmentSnapshotV1,
        journal: &ProjectCatalogTransactionJournalV1,
        locks: &[CatalogDirectoryLockGuard],
    ) -> ProjectCatalogStoreResult<()> {
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout binding verification requires the complete registry",
            ));
        };
        let action_ids = journal
            .monotonic_checkout_identity_actions
            .iter()
            .map(|action| action.observation_id.as_str())
            .collect::<BTreeSet<_>>();
        for (observation_id, target) in &registry.checkout_identity_markers {
            if action_ids.contains(observation_id.as_str()) {
                continue;
            }
            let root = registry
                .checkout_root(observation_id)
                .expect("validated registry target has a checkout root");
            let checkout_id = attachments
                .attachments
                .values()
                .find(|attachment| Path::new(&attachment.checkout_dir) == root)
                .map(|attachment| attachment.checkout_id.as_str())
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "registered checkout root lacks attachment evidence",
                    )
                })?;
            let lane = checkout_lock_for(target, locks)?;
            if !lane
                .read_regular("checkout-id", 128)?
                .as_deref()
                .is_some_and(|bytes| checkout_marker_bytes_match(bytes, checkout_id))
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "existing checkout identity disagrees with attachment evidence",
                ));
            }
            lane.ensure_still_current()?;
        }
        Ok(())
    }

    fn execute_monotonic_checkout_actions_locked(
        &self,
        journal: &ProjectCatalogTransactionJournalV1,
        locks: &[CatalogDirectoryLockGuard],
    ) -> ProjectCatalogStoreResult<()> {
        if journal.monotonic_checkout_identity_actions.is_empty() {
            return Ok(());
        }
        let ParticipantRegistry::Migration(registry) = &self.registry else {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_registry_required",
                "checkout identity actions require the complete migration registry",
            ));
        };
        for action in &journal.monotonic_checkout_identity_actions {
            self.io
                .checkpoint(FaultPoint::MonotonicCheckoutIdentityAction)?;
            let target = registry
                .checkout_identity_target(&action.observation_id)
                .ok_or_else(|| {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_migration_registry",
                        "migration registry lacks a checkout identity target",
                    )
                })?;
            let lane = checkout_lock_for(&target, locks)?;
            self.ensure_checkout_local_gitignore(lane)?;
            match lane.read_regular("checkout-id", 128)? {
                None => {
                    lane.atomic_replace("checkout-id", action.planned_id.as_bytes())?;
                }
                Some(bytes) if bytes.is_empty() => {
                    lane.atomic_replace("checkout-id", action.planned_id.as_bytes())?;
                }
                Some(bytes) if checkout_marker_bytes_match(&bytes, &action.planned_id) => {}
                Some(_) => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_recovery_incomplete",
                        "checkout identity marker disagrees with the journaled id",
                    ));
                }
            }
            if !lane
                .read_regular("checkout-id", 128)?
                .as_deref()
                .is_some_and(|bytes| checkout_marker_bytes_match(bytes, &action.planned_id))
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "checkout identity marker changed during journaled installation",
                ));
            }
            lane.ensure_still_current()?;
            self.io
                .checkpoint(FaultPoint::MonotonicCheckoutIdentityAction)?;
        }
        Ok(())
    }

    fn ensure_checkout_local_gitignore(
        &self,
        lane: &CatalogDirectoryLockGuard,
    ) -> ProjectCatalogStoreResult<()> {
        if lane.read_regular(".gitignore", 64 * 1024)?.as_deref()
            != Some(CHECKOUT_LOCAL_GITIGNORE_BYTES)
        {
            lane.atomic_replace(".gitignore", CHECKOUT_LOCAL_GITIGNORE_BYTES)?;
        }
        if lane.read_regular(".gitignore", 64 * 1024)?.as_deref()
            != Some(CHECKOUT_LOCAL_GITIGNORE_BYTES)
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                "checkout-local gitignore changed during journaled installation",
            ));
        }
        lane.sync_all()?;
        lane.ensure_still_current()
    }

    fn target_for_role(&self, role: &ParticipantRoleV1) -> ProjectCatalogStoreResult<PathBuf> {
        match role {
            ParticipantRoleV1::Catalog => Ok(self.paths.catalog.clone()),
            ParticipantRoleV1::Attachments => Ok(self.paths.attachments.clone()),
            ParticipantRoleV1::MigrationMarker => match &self.registry {
                ParticipantRegistry::Migration(_) => Ok(self.paths.migration_marker.clone()),
                ParticipantRegistry::Regular => Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_registry_required",
                    "migration marker target requires the complete code-owned registry",
                )),
            },
            role => match &self.registry {
                ParticipantRegistry::Migration(registry) => {
                    registry.participant_target(role).ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_invalid_migration_registry",
                            "migration registry lacks a journaled participant target",
                        )
                    })
                }
                ParticipantRegistry::Regular => Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_migration_registry_required",
                    "migration participant target requires the complete code-owned registry",
                )),
            },
        }
    }
}

#[derive(Debug)]
struct PreparedPair {
    catalog: CatalogSnapshotV2,
    attachments: AttachmentSnapshotV1,
    catalog_bytes: Vec<u8>,
    attachment_bytes: Vec<u8>,
    catalog_sha256: Sha256Hex,
    attachments_sha256: Sha256Hex,
}

impl PreparedPair {
    fn new(
        catalog: CatalogSnapshotV2,
        attachments: AttachmentSnapshotV1,
    ) -> ProjectCatalogStoreResult<Self> {
        validate_catalog_attachments(&catalog, &attachments).map_err(contract_error)?;
        if catalog.epoch == 0 {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_zero_epoch",
                "strict snapshots cannot be prepared with epoch zero",
            ));
        }
        let catalog_bytes = encode_catalog_snapshot(&catalog).map_err(contract_error)?;
        let attachment_bytes = encode_attachment_snapshot(&attachments).map_err(contract_error)?;
        let catalog_sha256 = sha256(&catalog_bytes);
        let attachments_sha256 = sha256(&attachment_bytes);
        Ok(Self {
            catalog,
            attachments,
            catalog_bytes,
            attachment_bytes,
            catalog_sha256,
            attachments_sha256,
        })
    }

    fn into_state(self) -> ProjectCatalogState {
        ProjectCatalogState {
            epoch: self.catalog.epoch,
            catalog: Arc::new(self.catalog),
            attachments: Arc::new(self.attachments),
            catalog_sha256: self.catalog_sha256,
            attachments_sha256: self.attachments_sha256,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub(crate) struct Sha256Hex(String);

impl Sha256Hex {
    pub(crate) fn parse(value: String) -> ProjectCatalogStoreResult<Self> {
        if value.len() != 64
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_hash",
                "expected a lowercase SHA-256 value",
            ));
        }
        Ok(Self(value))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    #[allow(dead_code)] // P1-B constructor consumed by P1-C.
    pub(crate) fn digest(bytes: &[u8]) -> Self {
        sha256(bytes)
    }
}

impl fmt::Display for Sha256Hex {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Sha256Hex {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

pub(crate) fn sha256(bytes: &[u8]) -> Sha256Hex {
    Sha256Hex(hex::encode(Sha256::digest(bytes)))
}

fn legacy_project_source_absence_sha256() -> Sha256Hex {
    sha256(b"bbox-project-catalog-legacy-project-source-absent-v1")
}

fn publisher_source_absence_sha256() -> Sha256Hex {
    sha256(b"bbox-project-catalog-publisher-source-absent-v1")
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
struct ValidatedBasename(String);

impl ValidatedBasename {
    fn parse(value: String) -> ProjectCatalogStoreResult<Self> {
        if !valid_basename(&value)
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_artifact_name",
                "transaction artifact name is not a validated basename",
            ));
        }
        Ok(Self(value))
    }

    fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ValidatedBasename {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        Self::parse(String::deserialize(deserializer)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ParticipantRoleV1 {
    Catalog,
    Attachments,
    EffectiveSourceManifest,
    Activation {
        project_id: ProjectId,
    },
    StoredGenerationMetadata {
        project_id: ProjectId,
        published_scope: PublishedScope,
        generation_id: Sha256Hex,
    },
    CollisionRetirement {
        project_id: ProjectId,
    },
    AcceptedPublicationPointer {
        project_id: ProjectId,
    },
    MigrationMarker,
}

impl ParticipantRoleV1 {
    pub(crate) fn artifact_token(&self) -> String {
        match self {
            Self::Catalog => "catalog".into(),
            Self::Attachments => "attachments".into(),
            _ => {
                let encoded =
                    serde_json::to_vec(self).expect("closed participant role must serialize");
                format!("role-{}", &sha256(&encoded).as_str()[..24])
            }
        }
    }

    fn max_bytes(&self) -> usize {
        match self {
            Self::Catalog => MAX_LEGACY_PROJECT_STORE_BYTES,
            Self::Attachments => MAX_PROJECT_CATALOG_BYTES,
            Self::EffectiveSourceManifest => MAX_CODE_SOURCE_EFFECTIVE_MANIFEST_BYTES,
            Self::Activation { .. } => MAX_CODE_SOURCE_ACTIVATION_BYTES,
            Self::StoredGenerationMetadata { .. } => MAX_CODE_SOURCE_GENERATION_METADATA_BYTES,
            Self::CollisionRetirement { .. } => MAX_CODE_SOURCE_COLLISION_RETIREMENT_BYTES,
            Self::AcceptedPublicationPointer { .. } => MAX_ACCEPTED_PUBLICATION_POINTER_BYTES,
            Self::MigrationMarker => MAX_MARKER_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum ArtifactKind {
    Backup,
    Stage,
}

fn artifact_name(
    transaction_id: &ProjectCatalogTransactionId,
    role: &ParticipantRoleV1,
    hash: &Sha256Hex,
    kind: ArtifactKind,
) -> ProjectCatalogStoreResult<ValidatedBasename> {
    let suffix = match kind {
        ArtifactKind::Backup => "bak",
        ArtifactKind::Stage => "stage",
    };
    ValidatedBasename::parse(format!(
        "{}.{}.{}.{}",
        transaction_id,
        role.artifact_token(),
        hash,
        suffix
    ))
}

fn validate_expected_artifact_name(
    transaction_id: &ProjectCatalogTransactionId,
    role: &ParticipantRoleV1,
    image: &ExpectedImageV1,
    kind: ArtifactKind,
) -> ProjectCatalogStoreResult<()> {
    let ExpectedImageV1::Present {
        sha256,
        artifact_name: actual,
    } = image
    else {
        return Ok(());
    };
    let expected = artifact_name(transaction_id, role, sha256, kind)?;
    if actual != &expected {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            "transaction artifact name is not code-derived",
        ));
    }
    Ok(())
}

fn build_transaction_participant(
    transaction_id: &ProjectCatalogTransactionId,
    role: ParticipantRoleV1,
    old_hash: Option<Sha256Hex>,
    post_image: &Option<Vec<u8>>,
) -> ProjectCatalogStoreResult<TransactionParticipantV1> {
    let old = match old_hash {
        Some(hash) => ExpectedImageV1::Present {
            artifact_name: artifact_name(transaction_id, &role, &hash, ArtifactKind::Backup)?,
            sha256: hash,
        },
        None => ExpectedImageV1::Absent {},
    };
    let new = match post_image {
        Some(bytes) => {
            let hash = sha256(bytes);
            ExpectedImageV1::Present {
                artifact_name: artifact_name(transaction_id, &role, &hash, ArtifactKind::Stage)?,
                sha256: hash,
            }
        }
        None => ExpectedImageV1::Absent {},
    };
    Ok(TransactionParticipantV1 { role, old, new })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum ExpectedImageV1 {
    Absent {},
    Present {
        sha256: Sha256Hex,
        artifact_name: ValidatedBasename,
    },
}

impl ExpectedImageV1 {
    fn sha256(&self) -> Option<&Sha256Hex> {
        match self {
            Self::Absent {} => None,
            Self::Present { sha256, .. } => Some(sha256),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TransactionParticipantV1 {
    role: ParticipantRoleV1,
    old: ExpectedImageV1,
    new: ExpectedImageV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionKindV1 {
    RegularPair,
    V1Migration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionStateV1 {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TransactionOutcomeV1 {
    Committed,
    RolledBack,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ImmutableAssetRoleV1 {
    LegacyProjectStoreBackup,
    LegacyPublisherRefBackup,
    /// Singleton asset closing the Phase 1 proof gap (Phase 3 plan
    /// section 4.2): the persisted `LegacyCommitNamespaceInventoryV1` rows
    /// the materializer later proves observed namespaces against. At most
    /// one per migration, like the two backups above.
    LegacyCommitNamespaceInventory,
    AcceptedPublicationGeneration {
        project_id: ProjectId,
        generation_id: AcceptedPublicationGenerationId,
    },
    CollectedGenerationManifest {
        published_scope: PublishedScope,
        generation_id: Sha256Hex,
    },
}

impl ImmutableAssetRoleV1 {
    pub(crate) fn artifact_token(&self) -> String {
        let encoded = serde_json::to_vec(self).expect("closed immutable role must serialize");
        format!("immutable-role-{}", &sha256(&encoded).as_str()[..24])
    }

    fn max_bytes(&self) -> usize {
        match self {
            Self::LegacyProjectStoreBackup => MAX_LEGACY_PROJECT_STORE_BYTES,
            Self::LegacyPublisherRefBackup => MAX_LEGACY_PUBLISHER_REF_SOURCE_BYTES,
            Self::LegacyCommitNamespaceInventory => {
                MAX_LEGACY_COMMIT_NAMESPACE_INVENTORY_ASSET_BYTES
            }
            Self::AcceptedPublicationGeneration { .. } => MAX_ACCEPTED_PUBLICATION_GENERATION_BYTES,
            Self::CollectedGenerationManifest { .. } => MAX_CODE_SOURCE_COLLECTED_MANIFEST_BYTES,
        }
    }

    fn required_mode(&self) -> ImmutableAssetModeV1 {
        match self {
            Self::LegacyProjectStoreBackup
            | Self::LegacyPublisherRefBackup
            | Self::LegacyCommitNamespaceInventory
            | Self::AcceptedPublicationGeneration { .. } => ImmutableAssetModeV1::Installable,
            Self::CollectedGenerationManifest { .. } => ImmutableAssetModeV1::PinnedExisting,
        }
    }
}

fn immutable_target_name(
    transaction_id: &ProjectCatalogTransactionId,
    role: &ImmutableAssetRoleV1,
    hash: &Sha256Hex,
) -> ProjectCatalogStoreResult<ValidatedBasename> {
    match role {
        ImmutableAssetRoleV1::LegacyProjectStoreBackup
        | ImmutableAssetRoleV1::LegacyPublisherRefBackup
        | ImmutableAssetRoleV1::LegacyCommitNamespaceInventory => {
            ValidatedBasename::parse(format!(
                "{}.{}.{}.immutable",
                transaction_id,
                role.artifact_token(),
                hash
            ))
        }
        ImmutableAssetRoleV1::AcceptedPublicationGeneration { generation_id, .. } => {
            ValidatedBasename::parse(format!("{generation_id}.json"))
        }
        ImmutableAssetRoleV1::CollectedGenerationManifest { .. } => {
            ValidatedBasename::parse("manifest.jsonl".to_string())
        }
    }
}

fn immutable_stage_name(
    transaction_id: &ProjectCatalogTransactionId,
    role: &ImmutableAssetRoleV1,
    hash: &Sha256Hex,
) -> ProjectCatalogStoreResult<ValidatedBasename> {
    ValidatedBasename::parse(format!(
        "{}.{}.{}.stage",
        transaction_id,
        role.artifact_token(),
        hash
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ImmutableAssetModeV1 {
    Installable,
    PinnedExisting,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ImmutableAssetV1 {
    role: ImmutableAssetRoleV1,
    mode: ImmutableAssetModeV1,
    sha256: Sha256Hex,
    validated_name: ValidatedBasename,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stage_name: Option<ValidatedBasename>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckoutIdentityActionV1 {
    observation_id: String,
    planned_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCatalogTransactionJournalV1 {
    version: u32,
    transaction_id: ProjectCatalogTransactionId,
    kind: TransactionKindV1,
    state: TransactionStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    outcome: Option<TransactionOutcomeV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    plan_hash: Option<Sha256Hex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    report_artifact_sha256: Option<Sha256Hex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolution_artifact_sha256: Option<Sha256Hex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    legacy_project_source: Option<MigrationLegacyProjectSourceEvidenceV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publisher_ref_source: Option<MigrationPublisherSourceEvidenceV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    publisher_pins: Vec<PublisherPinEvidenceV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    publisher_dispositions: Vec<PublisherDispositionEvidenceV1>,
    // Optional only so pre-P1-C regular journals retain their exact wire
    // shape. V1Migration validation below requires this field to be present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    resolved_quarantine_bindings: Option<BTreeSet<(ProjectId, Sha256Hex)>>,
    old_epoch: u64,
    new_epoch: u64,
    participants: Vec<TransactionParticipantV1>,
    immutable_assets: Vec<ImmutableAssetV1>,
    monotonic_checkout_identity_actions: Vec<CheckoutIdentityActionV1>,
    created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    committed_at: Option<u64>,
}

impl ProjectCatalogTransactionJournalV1 {
    fn validate(&self) -> ProjectCatalogStoreResult<()> {
        if self.version != JOURNAL_VERSION
            || self.old_epoch.checked_add(1) != Some(self.new_epoch)
            || self.created_at == 0
            || self.participants.len() > MAX_MIGRATION_PARTICIPANTS
            || self.immutable_assets.len() > MAX_MIGRATION_IMMUTABLE_ASSETS
            || self.publisher_pins.len() > MAX_MIGRATION_PUBLISHER_PINS
            || self.publisher_dispositions.len() > MAX_MIGRATION_PUBLISHER_PINS
            || self
                .resolved_quarantine_bindings
                .as_ref()
                .is_some_and(|bindings| bindings.len() > MAX_MIGRATION_INVENTORY_GENERATIONS)
            || self.monotonic_checkout_identity_actions.len() > MAX_MIGRATION_CHECKOUT_ACTIONS
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_journal",
                "journal header is invalid",
            ));
        }
        validate_durable_structural_evidence(
            &self.participants,
            &self.immutable_assets,
            &self.monotonic_checkout_identity_actions,
            "transaction journal",
        )?;
        match (self.state, self.outcome, self.committed_at) {
            (TransactionStateV1::Prepared, None, None)
            | (TransactionStateV1::Committed, Some(_), Some(_)) => {}
            _ => {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "journal state, outcome, and timestamps disagree",
                ));
            }
        }
        let roles = self
            .participants
            .iter()
            .map(|participant| participant.role.clone())
            .collect::<BTreeSet<_>>();
        if roles.len() != self.participants.len() {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_journal",
                "journal contains duplicate participant roles",
            ));
        }
        for participant in &self.participants {
            validate_expected_artifact_name(
                &self.transaction_id,
                &participant.role,
                &participant.old,
                ArtifactKind::Backup,
            )?;
            validate_expected_artifact_name(
                &self.transaction_id,
                &participant.role,
                &participant.new,
                ArtifactKind::Stage,
            )?;
        }
        for asset in &self.immutable_assets {
            let expected_target =
                immutable_target_name(&self.transaction_id, &asset.role, &asset.sha256)?;
            let expected_stage = match asset.mode {
                ImmutableAssetModeV1::Installable => Some(immutable_stage_name(
                    &self.transaction_id,
                    &asset.role,
                    &asset.sha256,
                )?),
                ImmutableAssetModeV1::PinnedExisting => None,
            };
            if asset.validated_name != expected_target
                || asset.stage_name != expected_stage
                || asset.mode != asset.role.required_mode()
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_invalid_journal",
                    "journal immutable asset names are not code-derived",
                ));
            }
        }
        match self.kind {
            TransactionKindV1::RegularPair => {
                let required =
                    BTreeSet::from([ParticipantRoleV1::Catalog, ParticipantRoleV1::Attachments]);
                let old_images_match_epoch = self.participants.iter().all(|participant| {
                    if self.old_epoch == 0 {
                        matches!(&participant.old, ExpectedImageV1::Absent {})
                    } else {
                        matches!(&participant.old, ExpectedImageV1::Present { .. })
                    }
                });
                if roles != required
                    || self.plan_hash.is_some()
                    || self.report_artifact_sha256.is_some()
                    || self.resolution_artifact_sha256.is_some()
                    || self.legacy_project_source.is_some()
                    || self.publisher_ref_source.is_some()
                    || !self.publisher_pins.is_empty()
                    || !self.publisher_dispositions.is_empty()
                    || self.resolved_quarantine_bindings.is_some()
                    || !self.immutable_assets.is_empty()
                    || !self.monotonic_checkout_identity_actions.is_empty()
                    || !old_images_match_epoch
                    || self
                        .participants
                        .iter()
                        .any(|participant| participant.new.sha256().is_none())
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_journal",
                        "regular transaction journal has an invalid participant set",
                    ));
                }
            }
            TransactionKindV1::V1Migration => {
                let resolved_quarantine_bindings =
                    self.resolved_quarantine_bindings.as_ref().ok_or_else(|| {
                        ProjectCatalogStoreError::new(
                            "error.project_catalog_invalid_journal",
                            "migration journal lacks canonical quarantine bindings",
                        )
                    })?;
                let mandatory = [
                    ParticipantRoleV1::Catalog,
                    ParticipantRoleV1::Attachments,
                    ParticipantRoleV1::EffectiveSourceManifest,
                    ParticipantRoleV1::MigrationMarker,
                ];
                let asset_roles = self
                    .immutable_assets
                    .iter()
                    .map(|asset| asset.role.clone())
                    .collect::<BTreeSet<_>>();
                let participant_for = |role: &ParticipantRoleV1| {
                    self.participants
                        .iter()
                        .find(|participant| &participant.role == role)
                };
                let catalog = participant_for(&ParticipantRoleV1::Catalog);
                let attachments = participant_for(&ParticipantRoleV1::Attachments);
                let marker = participant_for(&ParticipantRoleV1::MigrationMarker);
                let source_backup = self
                    .immutable_assets
                    .iter()
                    .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyProjectStoreBackup);
                let publisher_backup = self
                    .immutable_assets
                    .iter()
                    .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyPublisherRefBackup);
                let collision_projects = self
                    .participants
                    .iter()
                    .filter_map(|participant| {
                        if let ParticipantRoleV1::CollisionRetirement { project_id } =
                            &participant.role
                        {
                            Some(project_id.clone())
                        } else {
                            None
                        }
                    })
                    .collect::<BTreeSet<_>>();
                let participant_collision_bindings = self
                    .participants
                    .iter()
                    .filter_map(|participant| match &participant.role {
                        ParticipantRoleV1::StoredGenerationMetadata {
                            project_id,
                            generation_id,
                            ..
                        } if collision_projects.contains(project_id) => {
                            Some((project_id.clone(), generation_id.clone()))
                        }
                        _ => None,
                    })
                    .collect::<BTreeSet<_>>();
                let role_shapes_are_valid =
                    self.participants
                        .iter()
                        .all(|participant| match &participant.role {
                            ParticipantRoleV1::StoredGenerationMetadata { .. } => {
                                matches!(&participant.old, ExpectedImageV1::Present { .. })
                                    && matches!(
                                        &participant.new,
                                        ExpectedImageV1::Present { .. }
                                            | ExpectedImageV1::Absent {}
                                    )
                            }
                            ParticipantRoleV1::Activation { project_id } => {
                                matches!(&participant.new, ExpectedImageV1::Present { .. })
                                    || (matches!(&participant.old, ExpectedImageV1::Present { .. })
                                        && collision_projects.contains(project_id))
                            }
                            ParticipantRoleV1::CollisionRetirement { project_id } => {
                                matches!(
                                    &participant.old,
                                    ExpectedImageV1::Absent {} | ExpectedImageV1::Present { .. }
                                ) && matches!(&participant.new, ExpectedImageV1::Present { .. })
                                    && participant_for(&ParticipantRoleV1::Activation {
                                        project_id: project_id.clone(),
                                    })
                                    .is_none_or(
                                        |activation| {
                                            matches!(
                                                &activation.old,
                                                ExpectedImageV1::Present { .. }
                                            ) && matches!(
                                                &activation.new,
                                                ExpectedImageV1::Absent {}
                                            )
                                        },
                                    )
                                    && self.participants.iter().any(|stored| {
                                        matches!(
                                            &stored.role,
                                            ParticipantRoleV1::StoredGenerationMetadata {
                                                project_id: stored_project,
                                                ..
                                            } if stored_project == project_id
                                        ) && matches!(&stored.old, ExpectedImageV1::Present { .. })
                                            && matches!(
                                                &stored.new,
                                                ExpectedImageV1::Present { .. }
                                            )
                                    })
                            }
                            ParticipantRoleV1::AcceptedPublicationPointer { .. } => {
                                matches!(&participant.old, ExpectedImageV1::Absent {})
                                    && matches!(&participant.new, ExpectedImageV1::Present { .. })
                            }
                            ParticipantRoleV1::Catalog
                            | ParticipantRoleV1::Attachments
                            | ParticipantRoleV1::EffectiveSourceManifest
                            | ParticipantRoleV1::MigrationMarker => true,
                        });
                let action_ids = self
                    .monotonic_checkout_identity_actions
                    .iter()
                    .map(|action| action.observation_id.as_str())
                    .collect::<BTreeSet<_>>();
                if self.plan_hash.is_none()
                    || self.report_artifact_sha256.is_none()
                    || self.resolution_artifact_sha256.is_none()
                    || self.legacy_project_source.is_none()
                    || self.publisher_ref_source.is_none()
                    || self.old_epoch != 0
                    || self.new_epoch != 1
                    || mandatory.iter().any(|role| !roles.contains(role))
                    || self.participants.iter().any(|participant| {
                        mandatory.contains(&participant.role) && participant.new.sha256().is_none()
                    })
                    || asset_roles.len() != self.immutable_assets.len()
                    || match self.legacy_project_source.as_ref() {
                        Some(MigrationLegacyProjectSourceEvidenceV1::Missing {
                            absence_sha256,
                        }) => {
                            absence_sha256 != &legacy_project_source_absence_sha256()
                                || source_backup.is_some()
                                || !catalog.is_some_and(|participant| {
                                    matches!(&participant.old, ExpectedImageV1::Absent {})
                                })
                        }
                        Some(MigrationLegacyProjectSourceEvidenceV1::Present { sha256 }) => {
                            source_backup.map(|asset| &asset.sha256) != Some(sha256)
                                || catalog.and_then(|participant| participant.old.sha256())
                                    != Some(sha256)
                        }
                        None => true,
                    }
                    || match self.publisher_ref_source.as_ref() {
                        Some(MigrationPublisherSourceEvidenceV1::Missing { absence_sha256 }) => {
                            absence_sha256 != &publisher_source_absence_sha256()
                                || publisher_backup.is_some()
                                || !self.publisher_pins.is_empty()
                                || !self.publisher_dispositions.is_empty()
                        }
                        Some(MigrationPublisherSourceEvidenceV1::Present { sha256 }) => {
                            publisher_backup.map(|asset| &asset.sha256) != Some(sha256)
                        }
                        None => true,
                    }
                    || !role_shapes_are_valid
                    || resolved_quarantine_bindings != &participant_collision_bindings
                    || !catalog.is_some_and(|participant| {
                        matches!(&participant.new, ExpectedImageV1::Present { .. })
                    })
                    || !attachments.is_some_and(|participant| {
                        matches!(&participant.old, ExpectedImageV1::Absent {})
                            && matches!(&participant.new, ExpectedImageV1::Present { .. })
                    })
                    || !marker.is_some_and(|participant| {
                        matches!(&participant.old, ExpectedImageV1::Absent {})
                            && matches!(&participant.new, ExpectedImageV1::Present { .. })
                    })
                    || action_ids.len() != self.monotonic_checkout_identity_actions.len()
                    || self
                        .monotonic_checkout_identity_actions
                        .iter()
                        .any(|action| {
                            action.observation_id.is_empty()
                                || action.observation_id.len() > 256
                                || action.observation_id.chars().any(char::is_control)
                                || action.planned_id.len() != 32
                                || !action.planned_id.bytes().all(|byte| {
                                    byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()
                                })
                        })
                {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_invalid_journal",
                        "migration journal lacks its closed participant or evidence set",
                    ));
                }
                validate_publisher_evidence(
                    &self.publisher_pins,
                    &self.publisher_dispositions,
                    "journal",
                )?;
                self.legacy_project_source
                    .as_ref()
                    .expect("migration legacy project source presence was checked")
                    .validate()?;
                self.publisher_ref_source
                    .as_ref()
                    .expect("migration publisher source presence was checked")
                    .validate()?;
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCatalogMigrationMarkerV1 {
    version: u32,
    transaction_id: ProjectCatalogTransactionId,
    plan_hash: Sha256Hex,
    report_artifact_sha256: Sha256Hex,
    resolution_artifact_sha256: Sha256Hex,
    legacy_project_source: MigrationLegacyProjectSourceEvidenceV1,
    publisher_ref_source: MigrationPublisherSourceEvidenceV1,
    inventory_sha256: Sha256Hex,
    publisher_pins: Vec<PublisherPinEvidenceV1>,
    publisher_dispositions: Vec<PublisherDispositionEvidenceV1>,
    participants: Vec<MigrationParticipantEvidenceV1>,
    immutable_assets: Vec<MigrationImmutableAssetEvidenceV1>,
    migration_epoch: u64,
}

impl ProjectCatalogMigrationMarkerV1 {
    fn validate(&self) -> ProjectCatalogStoreResult<()> {
        if self.version != MIGRATION_MARKER_VERSION
            || self.migration_epoch != 1
            || self.participants.len() > MAX_MIGRATION_PARTICIPANTS
            || self.immutable_assets.len() > MAX_MIGRATION_IMMUTABLE_ASSETS
            || self.publisher_pins.len() > MAX_MIGRATION_PUBLISHER_PINS
            || self.publisher_dispositions.len() > MAX_MIGRATION_PUBLISHER_PINS
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker is invalid",
            ));
        }
        // The marker omits checkout actions and installable-stage names. It
        // validates its own projection here; the matching transaction journal
        // charges the strictly larger participant/asset/action superset and is
        // authoritative for the complete transaction budget.
        validate_durable_structural_evidence(
            &self.participants,
            &self.immutable_assets,
            &[] as &[CheckoutIdentityActionV1],
            "migration marker",
        )?;
        let participant_roles = self
            .participants
            .iter()
            .map(|evidence| evidence.role.clone())
            .collect::<BTreeSet<_>>();
        let participant_for = |role: &ParticipantRoleV1| {
            self.participants
                .iter()
                .find(|participant| &participant.role == role)
        };
        let collision_projects = self
            .participants
            .iter()
            .filter_map(|participant| {
                if let ParticipantRoleV1::CollisionRetirement { project_id } = &participant.role {
                    Some(project_id.clone())
                } else {
                    None
                }
            })
            .collect::<BTreeSet<_>>();
        let role_shapes_are_valid = self
            .participants
            .iter()
            .all(|participant| match &participant.role {
                ParticipantRoleV1::StoredGenerationMetadata { .. } => {
                    matches!(&participant.old, ExpectedImageV1::Present { .. })
                        && matches!(
                            &participant.new,
                            ExpectedImageV1::Present { .. } | ExpectedImageV1::Absent {}
                        )
                }
                ParticipantRoleV1::Activation { project_id } => {
                    matches!(&participant.new, ExpectedImageV1::Present { .. })
                        || (matches!(&participant.old, ExpectedImageV1::Present { .. })
                            && collision_projects.contains(project_id))
                }
                ParticipantRoleV1::CollisionRetirement { project_id } => {
                    matches!(
                        &participant.old,
                        ExpectedImageV1::Absent {} | ExpectedImageV1::Present { .. }
                    ) && matches!(&participant.new, ExpectedImageV1::Present { .. })
                        && participant_for(&ParticipantRoleV1::Activation {
                            project_id: project_id.clone(),
                        })
                        .is_none_or(|activation| {
                            matches!(&activation.old, ExpectedImageV1::Present { .. })
                                && matches!(&activation.new, ExpectedImageV1::Absent {})
                        })
                        && self.participants.iter().any(|stored| {
                            matches!(
                                &stored.role,
                                ParticipantRoleV1::StoredGenerationMetadata {
                                    project_id: stored_project,
                                    ..
                                } if stored_project == project_id
                            ) && matches!(&stored.old, ExpectedImageV1::Present { .. })
                                && matches!(&stored.new, ExpectedImageV1::Present { .. })
                        })
                }
                ParticipantRoleV1::AcceptedPublicationPointer { .. } => {
                    matches!(&participant.old, ExpectedImageV1::Absent {})
                        && matches!(&participant.new, ExpectedImageV1::Present { .. })
                }
                ParticipantRoleV1::Catalog
                | ParticipantRoleV1::Attachments
                | ParticipantRoleV1::EffectiveSourceManifest => true,
                ParticipantRoleV1::MigrationMarker => false,
            });
        if participant_roles.len() != self.participants.len()
            || !participant_roles.contains(&ParticipantRoleV1::Catalog)
            || !participant_roles.contains(&ParticipantRoleV1::Attachments)
            || !participant_roles.contains(&ParticipantRoleV1::EffectiveSourceManifest)
            || participant_roles.contains(&ParticipantRoleV1::MigrationMarker)
            || !role_shapes_are_valid
            || self.participants.iter().any(|evidence| {
                validate_expected_artifact_name(
                    &self.transaction_id,
                    &evidence.role,
                    &evidence.old,
                    ArtifactKind::Backup,
                )
                .is_err()
                    || validate_expected_artifact_name(
                        &self.transaction_id,
                        &evidence.role,
                        &evidence.new,
                        ArtifactKind::Stage,
                    )
                    .is_err()
            })
            || self.participants.iter().any(|evidence| {
                matches!(
                    &evidence.role,
                    ParticipantRoleV1::Catalog
                        | ParticipantRoleV1::Attachments
                        | ParticipantRoleV1::EffectiveSourceManifest
                ) && evidence.new.sha256().is_none()
            })
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker participant evidence is incomplete or duplicated",
            ));
        }
        let asset_roles = self
            .immutable_assets
            .iter()
            .map(|evidence| evidence.role.clone())
            .collect::<BTreeSet<_>>();
        if asset_roles.len() != self.immutable_assets.len()
            || self.immutable_assets.iter().any(|evidence| {
                match immutable_target_name(&self.transaction_id, &evidence.role, &evidence.sha256)
                {
                    Ok(expected) => {
                        expected != evidence.validated_name
                            || evidence.mode != evidence.role.required_mode()
                    }
                    Err(_) => true,
                }
            })
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker immutable evidence is incomplete or duplicated",
            ));
        }
        let source_backup = self
            .immutable_assets
            .iter()
            .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyProjectStoreBackup);
        let publisher_backup = self
            .immutable_assets
            .iter()
            .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyPublisherRefBackup);
        if match &self.publisher_ref_source {
            MigrationPublisherSourceEvidenceV1::Missing { absence_sha256 } => {
                absence_sha256 != &publisher_source_absence_sha256()
                    || publisher_backup.is_some()
                    || !self.publisher_pins.is_empty()
                    || !self.publisher_dispositions.is_empty()
            }
            MigrationPublisherSourceEvidenceV1::Present { sha256 } => {
                publisher_backup.map(|asset| &asset.sha256) != Some(sha256)
            }
        } {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker publisher source evidence disagrees with its backup state",
            ));
        }
        let catalog_backup = self
            .participants
            .iter()
            .find(|evidence| evidence.role == ParticipantRoleV1::Catalog)
            .and_then(|evidence| evidence.old.sha256());
        if match &self.legacy_project_source {
            MigrationLegacyProjectSourceEvidenceV1::Missing { absence_sha256 } => {
                absence_sha256 != &legacy_project_source_absence_sha256()
                    || source_backup.is_some()
                    || catalog_backup.is_some()
            }
            MigrationLegacyProjectSourceEvidenceV1::Present { sha256 } => {
                source_backup.map(|asset| &asset.sha256) != Some(sha256)
                    || catalog_backup != Some(sha256)
            }
        } {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_migration_incomplete",
                "migration marker legacy project source evidence disagrees with catalog and backup state",
            ));
        }
        self.legacy_project_source.validate()?;
        validate_publisher_evidence(&self.publisher_pins, &self.publisher_dispositions, "marker")?;
        Ok(())
    }
}

fn verify_migration_marker_journal_binding(
    marker: &ProjectCatalogMigrationMarkerV1,
    marker_bytes: &[u8],
    journal: &ProjectCatalogTransactionJournalV1,
) -> ProjectCatalogStoreResult<()> {
    marker.validate()?;
    journal.validate()?;
    let marker_hash = journal
        .participants
        .iter()
        .find(|participant| participant.role == ParticipantRoleV1::MigrationMarker)
        .and_then(|participant| participant.new.sha256());
    let journal_participants = journal
        .participants
        .iter()
        .filter(|participant| participant.role != ParticipantRoleV1::MigrationMarker)
        .map(|participant| MigrationParticipantEvidenceV1 {
            role: participant.role.clone(),
            old: participant.old.clone(),
            new: participant.new.clone(),
        })
        .collect::<Vec<_>>();
    let journal_immutable_assets = journal
        .immutable_assets
        .iter()
        .map(|asset| MigrationImmutableAssetEvidenceV1 {
            role: asset.role.clone(),
            mode: asset.mode,
            sha256: asset.sha256.clone(),
            validated_name: asset.validated_name.clone(),
        })
        .collect::<Vec<_>>();
    if journal.kind != TransactionKindV1::V1Migration
        || !matches!(
            (journal.state, journal.outcome),
            (TransactionStateV1::Prepared, None)
                | (
                    TransactionStateV1::Committed,
                    Some(TransactionOutcomeV1::Committed)
                )
        )
        || journal.transaction_id != marker.transaction_id
        || journal.plan_hash.as_ref() != Some(&marker.plan_hash)
        || journal.report_artifact_sha256.as_ref() != Some(&marker.report_artifact_sha256)
        || journal.resolution_artifact_sha256.as_ref() != Some(&marker.resolution_artifact_sha256)
        || journal.legacy_project_source.as_ref() != Some(&marker.legacy_project_source)
        || journal.publisher_ref_source.as_ref() != Some(&marker.publisher_ref_source)
        || journal.publisher_pins != marker.publisher_pins
        || journal.publisher_dispositions != marker.publisher_dispositions
        || journal.new_epoch != marker.migration_epoch
        || marker_hash != Some(&sha256(marker_bytes))
        || journal_participants != marker.participants
        || journal_immutable_assets != marker.immutable_assets
    {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_migration_incomplete",
            "migration marker disagrees with its retained transaction journal",
        ));
    }
    Ok(())
}

fn migration_artifact_identity_from_journal(
    journal: &ProjectCatalogTransactionJournalV1,
    marker: ProjectCatalogMigrationMarkerV1,
    observed_marker_sha256: Sha256Hex,
    migration_install_is_current: bool,
) -> ProjectCatalogStoreResult<MigrationArtifactIdentityV1> {
    let report_artifact_sha256 = journal.report_artifact_sha256.clone().ok_or_else(|| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            "migration journal lacks its reviewed report identity",
        )
    })?;
    let resolution_artifact_sha256 =
        journal.resolution_artifact_sha256.clone().ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_invalid_journal",
                "migration journal lacks its reviewed resolution identity",
            )
        })?;
    let plan_hash = journal.plan_hash.clone().ok_or_else(|| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            "migration journal lacks its plan identity",
        )
    })?;
    Ok(MigrationArtifactIdentityV1 {
        transaction_id: journal.transaction_id.clone(),
        plan_hash,
        inventory_sha256: marker.inventory_sha256,
        report_artifact_sha256,
        resolution_artifact_sha256,
        observed_marker_sha256,
        participants: journal
            .participants
            .iter()
            .map(|participant| MigrationParticipantArtifactIdentityV1 {
                role: participant.role.clone(),
                old_sha256: participant.old.sha256().cloned(),
                new_sha256: participant.new.sha256().cloned(),
            })
            .collect(),
        immutable_assets: journal
            .immutable_assets
            .iter()
            .map(|asset| MigrationImmutableAssetIdentityV1 {
                role: asset.role.clone(),
                sha256: asset.sha256.clone(),
            })
            .collect(),
        migration_install_is_current,
        epoch: journal.new_epoch,
        checkout_action_count: u64::try_from(journal.monotonic_checkout_identity_actions.len())
            .unwrap_or(u64::MAX),
        publisher_pin_count: u64::try_from(journal.publisher_pins.len()).unwrap_or(u64::MAX),
        quarantine_root_count: u64::try_from(
            journal
                .resolved_quarantine_bindings
                .iter()
                .flatten()
                .map(|(project_id, _)| project_id)
                .collect::<BTreeSet<_>>()
                .len(),
        )
        .unwrap_or(u64::MAX),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationParticipantEvidenceV1 {
    role: ParticipantRoleV1,
    old: ExpectedImageV1,
    new: ExpectedImageV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationImmutableAssetEvidenceV1 {
    role: ImmutableAssetRoleV1,
    mode: ImmutableAssetModeV1,
    sha256: Sha256Hex,
    validated_name: ValidatedBasename,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExpectedSide {
    Old,
    New,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RecoveryDecision {
    Forward,
    Rollback,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckoutActionRecoveryState {
    Compatible,
    ConflictingValid,
}

fn valid_checkout_identity_bytes(bytes: &[u8]) -> bool {
    let Ok(value) = std::str::from_utf8(bytes) else {
        return false;
    };
    let value = value.trim();
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FaultPoint {
    BackupWrite,
    BackupFsync,
    StageWrite,
    StageFsync,
    DirectoryFsync,
    PreparedJournalWrite,
    ParticipantInstall,
    RecoveryParticipantInstall,
    RecoveryParticipantRestore,
    RecoveryParticipantDelete,
    #[allow(dead_code)] // P1-B migration seam exercised once P1-C invokes it.
    ImmutableAssetWrite,
    #[allow(dead_code)] // P1-B migration seam exercised once P1-C invokes it.
    ImmutableAssetFsync,
    ImmutableAssetVerify,
    MonotonicCheckoutIdentityAction,
    Cleanup,
    CompletePlanVerify,
    CommittedJournalWrite,
}

struct CatalogDirectoryLockGuard {
    path: PathBuf,
    directory: NofollowDirectory,
}

impl CatalogDirectoryLockGuard {
    fn read_regular(
        &self,
        name: &str,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
        self.directory
            .read_regular(name, max_bytes, "checkout-local file")
            .map_err(|error| io_error("read checkout-local file under", &self.path, error))
    }

    fn atomic_replace(&self, name: &str, bytes: &[u8]) -> ProjectCatalogStoreResult<()> {
        self.directory
            .atomic_replace(name, bytes)
            .map_err(|error| io_error("replace checkout-local file under", &self.path, error))
    }

    fn sync_all(&self) -> ProjectCatalogStoreResult<()> {
        self.directory
            .sync_all()
            .map_err(|error| io_error("fsync checkout-local directory", &self.path, error))
    }

    fn ensure_still_current(&self) -> ProjectCatalogStoreResult<()> {
        self.directory
            .ensure_still_current()
            .map_err(|error| io_error("verify checkout-local directory", &self.path, error))
    }
}

fn checkout_lock_for<'a>(
    target: &Path,
    locks: &'a [CatalogDirectoryLockGuard],
) -> ProjectCatalogStoreResult<&'a CatalogDirectoryLockGuard> {
    let parent = target.parent().ok_or_else(|| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_unsafe_path",
            "checkout identity target has no parent",
        )
    })?;
    locks
        .iter()
        .find(|guard| guard.path == parent)
        .ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                "checkout identity directory lock is missing",
            )
        })
}

trait CatalogStoreIo: Send + Sync {
    fn acquire_mutation_lock(
        &self,
        catalog_path: &Path,
    ) -> ProjectCatalogStoreResult<StoreLockGuard>;
    fn read_regular_nofollow(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<Option<Vec<u8>>>;
    fn create_private_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()>;
    fn acquire_directory_lock_nofollow(
        &self,
        path: &Path,
    ) -> ProjectCatalogStoreResult<CatalogDirectoryLockGuard>;
    fn write_new_nofollow(&self, path: &Path, bytes: &[u8]) -> ProjectCatalogStoreResult<()>;
    fn fsync_regular_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()>;
    fn atomic_replace_sync_nofollow(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> ProjectCatalogStoreResult<()>;
    fn replace_from_stage_nofollow(
        &self,
        stage: &Path,
        target: &Path,
        expected_hash: &Sha256Hex,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<()>;
    fn remove_regular_exact(
        &self,
        path: &Path,
        expected_hash: &Sha256Hex,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<()>;
    fn remove_empty_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()>;
    fn fsync_dir(&self, path: &Path) -> ProjectCatalogStoreResult<()>;
    fn checkpoint(&self, _point: FaultPoint) -> ProjectCatalogStoreResult<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct RealCatalogStoreIo;

impl RealCatalogStoreIo {
    fn read_file(path: &Path, max_bytes: usize) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
        #[cfg(unix)]
        {
            return Self::read_file_unix(path, max_bytes);
        }
        #[cfg(not(unix))]
        {
            let mut options = OpenOptions::new();
            options.read(true);
            let mut file = match options.open(path) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(io_error("open", path, error)),
            };
            if !file
                .metadata()
                .map_err(|error| io_error("inspect", path, error))?
                .file_type()
                .is_file()
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_non_regular_file",
                    format!("{} is not a regular file", path.display()),
                ));
            }
            let limit = max_bytes.checked_add(1).ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_byte_limit",
                    "read byte limit overflowed",
                )
            })?;
            let mut bytes = Vec::new();
            std::io::Read::by_ref(&mut file)
                .take(limit as u64)
                .read_to_end(&mut bytes)
                .map_err(|error| io_error("read", path, error))?;
            if bytes.len() > max_bytes {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_byte_limit",
                    format!("{} exceeds its byte limit", path.display()),
                ));
            }
            Ok(Some(bytes))
        }
    }

    #[cfg(not(unix))]
    fn create_new_file(path: &Path) -> ProjectCatalogStoreResult<File> {
        let mut options = OpenOptions::new();
        options.create_new(true).read(true).write(true);
        options.open(path).map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_already_exists",
                    format!("{} already exists", path.display()),
                )
            } else {
                io_error("create", path, error)
            }
        })
    }

    #[cfg(not(unix))]
    fn unique_temp_path(path: &Path) -> PathBuf {
        let token = ProjectCatalogTransactionId::mint();
        path.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!(".project-catalog-replace-{token}.tmp"))
    }

    #[cfg(unix)]
    fn open_directory_unix(path: &Path, create_missing: bool) -> ProjectCatalogStoreResult<File> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::OpenOptionsExt;
        use std::path::Component;

        let mut directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(if path.is_absolute() { "/" } else { "." })
            .map_err(|error| io_error("open directory root for", path, error))?;
        for component in path.components() {
            let name = match component {
                Component::RootDir | Component::CurDir => continue,
                Component::Normal(name) => name,
                Component::Prefix(_) | Component::ParentDir => {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_unsafe_path",
                        format!("{} has an unsafe directory component", path.display()),
                    ));
                }
            };
            let name = CString::new(name.as_bytes()).map_err(|_| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_unsafe_path",
                    "directory component contains a NUL byte",
                )
            })?;
            let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
            let mut descriptor =
                unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            if descriptor < 0
                && create_missing
                && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT)
            {
                let created = unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) };
                if created < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.raw_os_error() != Some(libc::EEXIST) {
                        return Err(io_error("create directory component for", path, error));
                    }
                }
                directory
                    .sync_all()
                    .map_err(|error| io_error("fsync created directory parent for", path, error))?;
                descriptor = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
            }
            if descriptor < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) {
                    return Err(ProjectCatalogStoreError::new(
                        "error.project_catalog_path_not_found",
                        format!("directory component for {} is missing", path.display()),
                    ));
                }
                return Err(io_error("open directory component for", path, error));
            }
            directory = unsafe { File::from_raw_fd(descriptor) };
        }
        Ok(directory)
    }

    #[cfg(unix)]
    fn open_parent_unix(path: &Path) -> ProjectCatalogStoreResult<(File, std::ffi::CString)> {
        use std::os::unix::ffi::OsStrExt;

        let parent = path.parent().ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_unsafe_path",
                format!("{} has no parent directory", path.display()),
            )
        })?;
        let filename = path.file_name().ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_unsafe_path",
                format!("{} has no filename", path.display()),
            )
        })?;
        let filename = std::ffi::CString::new(filename.as_bytes()).map_err(|_| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_unsafe_path",
                "filename contains a NUL byte",
            )
        })?;
        Ok((Self::open_directory_unix(parent, false)?, filename))
    }

    #[cfg(unix)]
    fn read_file_unix(path: &Path, max_bytes: usize) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
        use std::os::fd::{AsRawFd, FromRawFd};

        if let Some(parent) = path.parent()
            && !path_exists_nofollow(parent)?
        {
            return Ok(None);
        }
        let (parent, filename) = Self::open_parent_unix(path)?;
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                filename.as_ptr(),
                libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(io_error("open", path, error));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        if !file
            .metadata()
            .map_err(|error| io_error("inspect", path, error))?
            .file_type()
            .is_file()
        {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_non_regular_file",
                format!("{} is not a regular file", path.display()),
            ));
        }
        let limit = max_bytes.checked_add(1).ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_byte_limit",
                "read byte limit overflowed",
            )
        })?;
        let mut bytes = Vec::new();
        std::io::Read::by_ref(&mut file)
            .take(limit as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| io_error("read", path, error))?;
        if bytes.len() > max_bytes {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_byte_limit",
                format!("{} exceeds its byte limit", path.display()),
            ));
        }
        Ok(Some(bytes))
    }

    #[cfg(unix)]
    fn write_new_file_unix(path: &Path, bytes: &[u8]) -> ProjectCatalogStoreResult<()> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let (parent, filename) = Self::open_parent_unix(path)?;
        let temp_name = CString::new(format!(
            ".project-catalog-new-{}",
            ProjectCatalogTransactionId::mint()
        ))
        .expect("code-owned temporary basename has no NUL");
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io_error(
                "create no-replace temporary file for",
                path,
                std::io::Error::last_os_error(),
            ));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        if let Err(error) = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error("write and fsync no-replace image for", path, error))
        {
            drop(file);
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0);
            }
            return Err(error);
        }
        drop(file);

        let linked = unsafe {
            libc::linkat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                parent.as_raw_fd(),
                filename.as_ptr(),
                0,
            )
        };
        let link_error = (linked < 0).then(std::io::Error::last_os_error);
        unsafe {
            libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0);
        }
        match link_error {
            None => Ok(()),
            Some(error) if error.raw_os_error() == Some(libc::EEXIST) => {
                Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_already_exists",
                    format!("{} already exists", path.display()),
                ))
            }
            Some(error) => Err(io_error("install no-replace image for", path, error)),
        }
    }

    #[cfg(unix)]
    fn atomic_replace_unix(path: &Path, bytes: &[u8]) -> ProjectCatalogStoreResult<()> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        let (parent, filename) = Self::open_parent_unix(path)?;
        let temp_name = CString::new(format!(
            ".project-catalog-replace-{}.tmp",
            ProjectCatalogTransactionId::mint()
        ))
        .expect("code-owned temporary basename has no NUL");
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if descriptor < 0 {
            return Err(io_error(
                "create transaction temporary file for",
                path,
                std::io::Error::last_os_error(),
            ));
        }
        let mut file = unsafe { File::from_raw_fd(descriptor) };
        if let Err(error) = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| io_error("write and fsync temporary file for", path, error))
        {
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0);
            }
            return Err(error);
        }
        drop(file);
        let renamed = unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                temp_name.as_ptr(),
                parent.as_raw_fd(),
                filename.as_ptr(),
            )
        };
        if renamed < 0 {
            let error = io_error("replace", path, std::io::Error::last_os_error());
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), temp_name.as_ptr(), 0);
            }
            return Err(error);
        }
        parent
            .sync_all()
            .map_err(|error| io_error("fsync directory for", path, error))
    }
}

impl CatalogStoreIo for RealCatalogStoreIo {
    fn acquire_mutation_lock(
        &self,
        catalog_path: &Path,
    ) -> ProjectCatalogStoreResult<StoreLockGuard> {
        acquire_store_lock_nofollow(catalog_path)
            .map_err(|error| io_error("acquire mutation lock for", catalog_path, error))
    }

    fn read_regular_nofollow(
        &self,
        path: &Path,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
        Self::read_file(path, max_bytes)
    }

    fn create_private_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            Self::open_directory_unix(path, true)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| io_error("create parent directory for", path, error))?;
            }
            match fs::create_dir(path) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                            .map_err(|error| io_error("set permissions on", path, error))?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error("create directory", path, error)),
            }
            let metadata = fs::symlink_metadata(path)
                .map_err(|error| io_error("inspect directory", path, error))?;
            if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_non_regular_file",
                    format!("{} is not a no-follow directory", path.display()),
                ));
            }
            Ok(())
        }
    }

    fn acquire_directory_lock_nofollow(
        &self,
        path: &Path,
    ) -> ProjectCatalogStoreResult<CatalogDirectoryLockGuard> {
        let directory = NofollowDirectory::open_existing(path)
            .map_err(|error| io_error("open directory lock", path, error))?
            .ok_or_else(|| {
                ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "checkout identity directory disappeared before lock acquisition",
                )
            })?;
        directory
            .lock_exclusive()
            .map_err(|error| io_error("acquire directory lock", path, error))?;
        Ok(CatalogDirectoryLockGuard {
            path: path.to_path_buf(),
            directory,
        })
    }

    fn write_new_nofollow(&self, path: &Path, bytes: &[u8]) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            return Self::write_new_file_unix(path, bytes);
        }
        #[cfg(not(unix))]
        {
            let temp = Self::unique_temp_path(path);
            let mut file = Self::create_new_file(&temp)?;
            if let Err(error) = file
                .write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| io_error("write and fsync no-replace image for", path, error))
            {
                drop(file);
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
            drop(file);
            let result = fs::hard_link(&temp, path).map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    ProjectCatalogStoreError::new(
                        "error.project_catalog_already_exists",
                        format!("{} already exists", path.display()),
                    )
                } else {
                    io_error("install no-replace image for", path, error)
                }
            });
            let _ = fs::remove_file(temp);
            result
        }
    }

    fn fsync_regular_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            use std::os::fd::{AsRawFd, FromRawFd};

            let (parent, filename) = Self::open_parent_unix(path)?;
            let descriptor = unsafe {
                libc::openat(
                    parent.as_raw_fd(),
                    filename.as_ptr(),
                    libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if descriptor < 0 {
                return Err(io_error(
                    "open for fsync",
                    path,
                    std::io::Error::last_os_error(),
                ));
            }
            let file = unsafe { File::from_raw_fd(descriptor) };
            if !file
                .metadata()
                .map_err(|error| io_error("inspect for fsync", path, error))?
                .file_type()
                .is_file()
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_non_regular_file",
                    format!("{} is not a regular file", path.display()),
                ));
            }
            return file
                .sync_all()
                .map_err(|error| io_error("fsync", path, error));
        }
        #[cfg(not(unix))]
        {
            let mut options = OpenOptions::new();
            options.read(true);
            let file = options
                .open(path)
                .map_err(|error| io_error("open for fsync", path, error))?;
            if !file
                .metadata()
                .map_err(|error| io_error("inspect for fsync", path, error))?
                .file_type()
                .is_file()
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_non_regular_file",
                    format!("{} is not a regular file", path.display()),
                ));
            }
            file.sync_all()
                .map_err(|error| io_error("fsync", path, error))
        }
    }

    fn atomic_replace_sync_nofollow(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            return Self::atomic_replace_unix(path, bytes);
        }
        #[cfg(not(unix))]
        {
            let temp = Self::unique_temp_path(path);
            let mut file = Self::create_new_file(&temp)?;
            if let Err(error) = file
                .write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| io_error("write and fsync", &temp, error))
            {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
            drop(file);
            if let Err(error) =
                fs::rename(&temp, path).map_err(|error| io_error("replace", path, error))
            {
                let _ = fs::remove_file(&temp);
                return Err(error);
            }
            self.fsync_dir(path.parent().expect("derived path has parent"))
        }
    }

    fn replace_from_stage_nofollow(
        &self,
        stage: &Path,
        target: &Path,
        expected_hash: &Sha256Hex,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<()> {
        let bytes = Self::read_file(stage, max_bytes)?.ok_or_else(|| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                format!("required stage {} is missing", stage.display()),
            )
        })?;
        if sha256(&bytes) != *expected_hash {
            return Err(ProjectCatalogStoreError::new(
                "error.project_catalog_recovery_incomplete",
                format!("required stage {} has unexpected bytes", stage.display()),
            ));
        }
        self.atomic_replace_sync_nofollow(target, &bytes)
    }

    fn remove_regular_exact(
        &self,
        path: &Path,
        expected_hash: &Sha256Hex,
        max_bytes: usize,
    ) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            use std::ffi::CString;
            use std::os::fd::AsRawFd;

            let (parent, filename) = Self::open_parent_unix(path)?;
            let quarantine = CString::new(format!(
                ".{}.{}.rollback",
                path.file_name()
                    .and_then(|name| name.to_str())
                    .unwrap_or("project-catalog"),
                ProjectCatalogTransactionId::mint()
            ))
            .expect("code-owned quarantine basename has no NUL");
            let moved = unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    filename.as_ptr(),
                    parent.as_raw_fd(),
                    quarantine.as_ptr(),
                )
            };
            if moved < 0 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ENOENT) {
                    return Ok(());
                }
                return Err(io_error("quarantine before removal", path, error));
            }
            let quarantine_path = path.parent().expect("derived path has parent").join(
                quarantine
                    .to_str()
                    .expect("code-owned quarantine basename is UTF-8"),
            );
            let actual = Self::read_file(&quarantine_path, max_bytes)?.map(|bytes| sha256(&bytes));
            if actual.as_ref() != Some(expected_hash) {
                let restored = unsafe {
                    libc::renameat(
                        parent.as_raw_fd(),
                        quarantine.as_ptr(),
                        parent.as_raw_fd(),
                        filename.as_ptr(),
                    )
                };
                let detail = if restored == 0 {
                    "unexpected participant was restored"
                } else {
                    "unexpected participant remains quarantined for inspection"
                };
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    detail,
                ));
            }
            let removed = unsafe { libc::unlinkat(parent.as_raw_fd(), quarantine.as_ptr(), 0) };
            if removed < 0 {
                return Err(io_error(
                    "remove verified rollback target",
                    path,
                    std::io::Error::last_os_error(),
                ));
            }
            return parent
                .sync_all()
                .map_err(|error| io_error("fsync directory for", path, error));
        }
        #[cfg(not(unix))]
        {
            let Some(bytes) = Self::read_file(path, max_bytes)? else {
                return Ok(());
            };
            if sha256(&bytes) != *expected_hash {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_recovery_incomplete",
                    "refused to remove participant with unexplained bytes",
                ));
            }
            fs::remove_file(path).map_err(|error| io_error("remove", path, error))
        }
    }

    fn remove_empty_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;

            let (parent, filename) = match Self::open_parent_unix(path) {
                Ok(value) => value,
                Err(error) if error.code() == "error.project_catalog_path_not_found" => {
                    return Ok(());
                }
                Err(error) => return Err(error),
            };
            let removed = unsafe {
                libc::unlinkat(parent.as_raw_fd(), filename.as_ptr(), libc::AT_REMOVEDIR)
            };
            if removed < 0 {
                let error = std::io::Error::last_os_error();
                // Cleanup owns the empty envelope, not retained evidence within it.
                if matches!(
                    error.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::ENOTEMPTY) | Some(libc::EEXIST)
                ) {
                    return Ok(());
                }
                return Err(io_error("remove empty directory", path, error));
            }
            return parent
                .sync_all()
                .map_err(|error| io_error("fsync directory for", path, error));
        }
        #[cfg(not(unix))]
        {
            match fs::remove_dir(path) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                // Cleanup owns the empty envelope, not retained evidence within it.
                Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => Ok(()),
                Err(error) => Err(io_error("remove empty directory", path, error)),
            }
        }
    }

    fn fsync_dir(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
        #[cfg(unix)]
        {
            return Self::open_directory_unix(path, false)?
                .sync_all()
                .map_err(|error| io_error("fsync directory", path, error));
        }
        #[cfg(not(unix))]
        {
            let mut options = OpenOptions::new();
            options.read(true);
            let directory = options
                .open(path)
                .map_err(|error| io_error("open directory for fsync", path, error))?;
            if !directory
                .metadata()
                .map_err(|error| io_error("inspect directory for fsync", path, error))?
                .file_type()
                .is_dir()
            {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_non_regular_file",
                    format!("{} is not a directory", path.display()),
                ));
            }
            directory
                .sync_all()
                .map_err(|error| io_error("fsync directory", path, error))
        }
    }
}

fn encode_bounded_json<T: Serialize>(
    value: &T,
    max_bytes: usize,
    label: &str,
) -> ProjectCatalogStoreResult<Vec<u8>> {
    let mut bytes = serde_json::to_vec_pretty(value).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            format!("could not encode {label}: {error}"),
        )
    })?;
    bytes.push(b'\n');
    if bytes.len() > max_bytes {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_byte_limit",
            format!("{label} exceeds its byte limit"),
        ));
    }
    Ok(bytes)
}

fn decode_bounded_json<T>(
    bytes: &[u8],
    max_bytes: usize,
    label: &str,
) -> ProjectCatalogStoreResult<T>
where
    T: for<'de> Deserialize<'de>,
{
    if bytes.len() > max_bytes {
        return Err(ProjectCatalogStoreError::new(
            "error.project_catalog_byte_limit",
            format!("{label} exceeds its byte limit"),
        ));
    }
    serde_json::from_slice(bytes).map_err(|error| {
        ProjectCatalogStoreError::new(
            "error.project_catalog_invalid_journal",
            format!("could not decode {label}: {error}"),
        )
    })
}

fn unix_timestamp() -> ProjectCatalogStoreResult<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs().max(1))
        .map_err(|error| {
            ProjectCatalogStoreError::new(
                "error.project_catalog_clock",
                format!("system clock precedes Unix epoch: {error}"),
            )
        })
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Barrier, Mutex};
    use std::time::Duration;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        ATTACHMENT_VERSION_V1, AttachmentCapabilities, AttachmentId, AttachmentKind,
        AttachmentStatus, CATALOG_VERSION_V2, CheckoutAttachment, CorpusProject,
        LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry, LegacyProjectRecordV1,
        LegacyProjectStoreV1, ProjectScope, ScopeMigrationAttachmentProof,
        ScopeMigrationAuthorityProvenance, ScopeMigrationId, ScopeMigrationKind,
        ScopeMigrationRecord,
    };
    use bbox_stores::store_persister::StorePersister;

    use super::*;

    fn historical_selector(project_id: &str, generation_id: &str) -> String {
        bbox_corpus_index::index::project_files::collected_materialization_selector(
            project_id,
            generation_id,
        )
    }

    fn write_unprotected_legacy_generation(
        paths: &CodeSourceStorePaths,
    ) -> (PublishedScope, String) {
        let scope = PublishedScope::try_new("unexplained-repo", ".").unwrap();
        let generation_id = write_unprotected_legacy_generation_in_scope(paths, scope.clone());
        (scope, generation_id)
    }

    fn write_unprotected_legacy_generation_in_scope(
        paths: &CodeSourceStorePaths,
        scope: PublishedScope,
    ) -> String {
        let entries = Vec::new();
        let head = "b".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let producer_id = "recovery-probe";
        let generation_id = bbox_code_source::generation_id(producer_id, &descriptor);
        let record = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: generation_id.clone(),
            producer_id: producer_id.into(),
            ordinal: 100,
            descriptor,
            state: bbox_code_source::GenerationState::Superseded,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: None,
            entity_inventory_sha256: None,
        };
        let metadata = paths.generation_metadata(&scope, &generation_id).unwrap();
        fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        fs::write(metadata, serde_json::to_vec_pretty(&record).unwrap()).unwrap();
        fs::write(
            paths.generation_manifest(&scope, &generation_id).unwrap(),
            [],
        )
        .unwrap();
        generation_id
    }

    #[derive(Debug)]
    struct TracingIo {
        real: RealCatalogStoreIo,
        fail_at: Option<usize>,
        fail_points: BTreeSet<FaultPoint>,
        fail_read_paths: BTreeSet<PathBuf>,
        seen: AtomicUsize,
        points: Mutex<Vec<FaultPoint>>,
        mutation_lock_paths: Mutex<Vec<PathBuf>>,
    }

    impl TracingIo {
        fn recording() -> Self {
            Self {
                real: RealCatalogStoreIo,
                fail_at: None,
                fail_points: BTreeSet::new(),
                fail_read_paths: BTreeSet::new(),
                seen: AtomicUsize::new(0),
                points: Mutex::new(Vec::new()),
                mutation_lock_paths: Mutex::new(Vec::new()),
            }
        }

        fn failing_at(index: usize) -> Self {
            Self {
                real: RealCatalogStoreIo,
                fail_at: Some(index),
                fail_points: BTreeSet::new(),
                fail_read_paths: BTreeSet::new(),
                seen: AtomicUsize::new(0),
                points: Mutex::new(Vec::new()),
                mutation_lock_paths: Mutex::new(Vec::new()),
            }
        }

        fn failing_points(points: impl IntoIterator<Item = FaultPoint>) -> Self {
            Self {
                real: RealCatalogStoreIo,
                fail_at: None,
                fail_points: points.into_iter().collect(),
                fail_read_paths: BTreeSet::new(),
                seen: AtomicUsize::new(0),
                points: Mutex::new(Vec::new()),
                mutation_lock_paths: Mutex::new(Vec::new()),
            }
        }

        fn failing_at_and_points(
            index: usize,
            points: impl IntoIterator<Item = FaultPoint>,
        ) -> Self {
            Self {
                real: RealCatalogStoreIo,
                fail_at: Some(index),
                fail_points: points.into_iter().collect(),
                fail_read_paths: BTreeSet::new(),
                seen: AtomicUsize::new(0),
                points: Mutex::new(Vec::new()),
                mutation_lock_paths: Mutex::new(Vec::new()),
            }
        }

        fn failing_reads(paths: impl IntoIterator<Item = PathBuf>) -> Self {
            Self {
                real: RealCatalogStoreIo,
                fail_at: None,
                fail_points: BTreeSet::new(),
                fail_read_paths: paths.into_iter().collect(),
                seen: AtomicUsize::new(0),
                points: Mutex::new(Vec::new()),
                mutation_lock_paths: Mutex::new(Vec::new()),
            }
        }

        fn trace(&self) -> Vec<FaultPoint> {
            self.points.lock().unwrap().clone()
        }

        fn mutation_lock_paths(&self) -> Vec<PathBuf> {
            self.mutation_lock_paths.lock().unwrap().clone()
        }
    }

    impl CatalogStoreIo for TracingIo {
        fn acquire_mutation_lock(
            &self,
            catalog_path: &Path,
        ) -> ProjectCatalogStoreResult<StoreLockGuard> {
            self.mutation_lock_paths
                .lock()
                .unwrap()
                .push(catalog_path.to_path_buf());
            self.real.acquire_mutation_lock(catalog_path)
        }

        fn read_regular_nofollow(
            &self,
            path: &Path,
            max_bytes: usize,
        ) -> ProjectCatalogStoreResult<Option<Vec<u8>>> {
            if self.fail_read_paths.contains(path) {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_injected_fault",
                    "injected artifact read failure",
                ));
            }
            self.real.read_regular_nofollow(path, max_bytes)
        }

        fn create_private_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
            self.real.create_private_dir_nofollow(path)
        }

        fn acquire_directory_lock_nofollow(
            &self,
            path: &Path,
        ) -> ProjectCatalogStoreResult<CatalogDirectoryLockGuard> {
            self.real.acquire_directory_lock_nofollow(path)
        }

        fn write_new_nofollow(&self, path: &Path, bytes: &[u8]) -> ProjectCatalogStoreResult<()> {
            self.real.write_new_nofollow(path, bytes)
        }

        fn fsync_regular_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
            self.real.fsync_regular_nofollow(path)
        }

        fn atomic_replace_sync_nofollow(
            &self,
            path: &Path,
            bytes: &[u8],
        ) -> ProjectCatalogStoreResult<()> {
            self.real.atomic_replace_sync_nofollow(path, bytes)
        }

        fn replace_from_stage_nofollow(
            &self,
            stage: &Path,
            target: &Path,
            expected_hash: &Sha256Hex,
            max_bytes: usize,
        ) -> ProjectCatalogStoreResult<()> {
            self.real
                .replace_from_stage_nofollow(stage, target, expected_hash, max_bytes)
        }

        fn remove_regular_exact(
            &self,
            path: &Path,
            expected_hash: &Sha256Hex,
            max_bytes: usize,
        ) -> ProjectCatalogStoreResult<()> {
            self.real
                .remove_regular_exact(path, expected_hash, max_bytes)
        }

        fn remove_empty_dir_nofollow(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
            self.real.remove_empty_dir_nofollow(path)
        }

        fn fsync_dir(&self, path: &Path) -> ProjectCatalogStoreResult<()> {
            self.real.fsync_dir(path)
        }

        fn checkpoint(&self, point: FaultPoint) -> ProjectCatalogStoreResult<()> {
            let index = self.seen.fetch_add(1, Ordering::SeqCst);
            self.points.lock().unwrap().push(point);
            if self.fail_at == Some(index) || self.fail_points.contains(&point) {
                return Err(ProjectCatalogStoreError::new(
                    "error.project_catalog_injected_fault",
                    format!("injected fault at checkpoint {index}: {point:?}"),
                ));
            }
            Ok(())
        }
    }

    fn projects_path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("projects.json");
        (directory, path)
    }

    fn assert_absent_pair(path: &Path) {
        let paths = ProjectCatalogPaths::derive(path).unwrap();
        assert!(!paths.catalog.exists());
        assert!(!paths.attachments.exists());
    }

    fn assert_no_migration_transaction_outputs(path: &Path) {
        let paths = ProjectCatalogPaths::derive(path).unwrap();
        for output in [
            paths.attachments,
            paths.journal,
            paths.migration_marker,
            paths.migration_receipt,
            paths.migration_assets_dir,
            paths.stage_dir,
            paths.backup_dir,
            path.parent().unwrap().join("checkout"),
        ] {
            assert!(
                !output.exists(),
                "validation unexpectedly created {}",
                output.display()
            );
        }
    }

    fn assert_no_migration_outputs(path: &Path) {
        assert_no_migration_transaction_outputs(path);
        let code_source_root = path.parent().unwrap().join("code-source");
        assert!(
            !code_source_root.exists(),
            "validation unexpectedly created {}",
            code_source_root.display()
        );
    }

    fn state_fingerprint(state: &ProjectCatalogState) -> (u64, String, String) {
        (
            state.epoch(),
            state.catalog_sha256().to_owned(),
            state.attachments_sha256().to_owned(),
        )
    }

    fn assert_known_state_or_absent(path: &Path, allowed_states: &[(u64, String, String)]) {
        match ProjectCatalogStore::open_existing(path.to_path_buf()) {
            Ok(store) => {
                let state = store.snapshot().unwrap();
                let actual = state_fingerprint(&state);
                assert!(
                    allowed_states.contains(&actual),
                    "unexpected recovered state {actual:?}"
                );
            }
            Err(error) if error.code() == "error.project_catalog_not_initialized" => {
                assert_absent_pair(path);
            }
            Err(error) => panic!("unexpected reopen error: {error}"),
        }
    }

    fn assert_known_migration_state_or_absent(
        path: &Path,
        registry: MigrationParticipantRegistry,
        expected_legacy_bytes: &[u8],
        allowed_states: &[(u64, String, String)],
    ) {
        let paths = ProjectCatalogPaths::derive(path).unwrap();
        if let Ok(catalog_bytes) = fs::read(path)
            && decode_legacy_project_store(&catalog_bytes).is_ok()
        {
            assert_eq!(catalog_bytes, expected_legacy_bytes);
            assert!(!paths.attachments.exists());
            let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(journal.state, TransactionStateV1::Committed);
            assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));
            return;
        }
        match ProjectCatalogStore::open_existing_after_migration(path.to_path_buf(), registry) {
            Ok(store) => {
                let state = store.snapshot().unwrap();
                let actual = state_fingerprint(&state);
                assert!(
                    allowed_states.contains(&actual),
                    "unexpected recovered migration state {actual:?}"
                );
            }
            Err(error) if error.code() == "error.project_catalog_not_initialized" => {
                assert_absent_pair(path);
            }
            Err(error) => panic!("unexpected migration reopen error: {error}"),
        }
    }

    fn assert_retained_journal_artifacts(path: &Path) {
        let paths = ProjectCatalogPaths::derive(path).unwrap();
        let Ok(bytes) = fs::read(&paths.journal) else {
            return;
        };
        let journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "transaction journal").unwrap();
        journal.validate().unwrap();
        assert_eq!(journal.state, TransactionStateV1::Committed);
        for participant in &journal.participants {
            for (root, image) in [
                (&paths.backup_dir, &participant.old),
                (&paths.stage_dir, &participant.new),
            ] {
                if let ExpectedImageV1::Present {
                    sha256: expected,
                    artifact_name,
                } = image
                {
                    let bytes = fs::read(root.join(artifact_name.as_str())).unwrap();
                    assert_eq!(sha256(&bytes), *expected);
                }
            }
        }
    }

    fn corrupt_staged_role(path: &Path, role: ParticipantRoleV1) {
        let paths = ProjectCatalogPaths::derive(path).unwrap();
        let bytes = fs::read(&paths.journal).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "transaction journal").unwrap();
        let participant = journal
            .participants
            .iter()
            .find(|participant| participant.role == role)
            .unwrap();
        let ExpectedImageV1::Present { artifact_name, .. } = &participant.new else {
            panic!("selected role has no staged post-image");
        };
        fs::write(
            paths.stage_dir.join(artifact_name.as_str()),
            b"corrupt stage",
        )
        .unwrap();
    }

    fn add_promoted_fixture(
        catalog: &mut CatalogSnapshotV2,
        attachments: &mut AttachmentSnapshotV1,
    ) -> ProjectCatalogStoreResult<()> {
        let project_id = ProjectId::parse("example").map_err(contract_error)?;
        let attachment_id =
            AttachmentId::parse("att_11111111111111111111111111111111").map_err(contract_error)?;
        let migration_id = ScopeMigrationId::parse("sm_22222222222222222222222222222222")
            .map_err(contract_error)?;
        let legacy_binding_id = LegacyPathBindingId::parse("lpb_33333333333333333333333333333333")
            .map_err(contract_error)?;
        let published_scope =
            PublishedScope::try_new("repo-example", ".").map_err(contract_error)?;
        let project = CorpusProject {
            project_id: project_id.clone(),
            scope: ProjectScope::Published(published_scope.clone()),
            operator_aliases: BTreeSet::new(),
            nominated_aliases: BTreeSet::new(),
            display_name: "Example project".into(),
            created_at: "2026-07-22T00:00:00Z".into(),
            registered_at_compat: None,
            repo_history: None,
            languages: BTreeSet::new(),
        };
        catalog.projects.insert(project_id.clone(), project);
        catalog.scope_migrations.insert(
            migration_id.clone(),
            ScopeMigrationRecord {
                scope_migration_id: migration_id.clone(),
                project_id: project_id.clone(),
                catalog_epoch: 2,
                authority_provenance: ScopeMigrationAuthorityProvenance::AttachmentProved,
                operator_invocation: "bbox_project_promote".into(),
                operator_reason: None,
                old_scope: ProjectScope::LegacyLocal,
                new_scope: ProjectScope::Published(published_scope.clone()),
                kind: ScopeMigrationKind::Promotion,
                migrated_at: "2026-07-22T00:00:00Z".into(),
                code_bridge_generation: None,
                publication_bridge_generation: None,
                pending_capabilities: BTreeSet::new(),
            },
        );
        attachments.attachments.insert(
            attachment_id.clone(),
            CheckoutAttachment {
                attachment_id: attachment_id.clone(),
                project_id: project_id.clone(),
                checkout_id: "44444444444444444444444444444444".into(),
                checkout_dir: "/tmp/example".into(),
                checkout_project_dir: "/tmp/example".into(),
                project_root_relpath: ".".into(),
                kind: AttachmentKind::Base,
                validated_scope: Some(published_scope.clone()),
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities {
                    local_code_source: true,
                    ..AttachmentCapabilities::default()
                },
                status: AttachmentStatus::Attached,
                attached_at: "2026-07-22T00:00:00Z".into(),
                detached_at: None,
            },
        );
        attachments.scope_migration_proofs.insert(
            migration_id.clone(),
            ScopeMigrationAttachmentProof {
                scope_migration_id: migration_id,
                attachment_id,
                checkout_id: "44444444444444444444444444444444".into(),
                old_scope: ProjectScope::LegacyLocal,
                new_scope: ProjectScope::Published(published_scope),
                proved_at: "2026-07-22T00:00:00Z".into(),
            },
        );
        attachments.legacy_path_bindings.insert(
            legacy_binding_id.clone(),
            LegacyPathLedgerEntry {
                legacy_path_binding_id: legacy_binding_id,
                historical_path: "/tmp/legacy-example".into(),
                source_store: "synthetic".into(),
                source_row_id: "row-1".into(),
                member_row_count: 1,
                member_commitment_sha256: "a".repeat(64),
                inventory_epoch: 1,
                status: LegacyPathBindingStatus::Unscoped {},
            },
        );
        Ok(())
    }

    fn basic_migration_draft(
        path: &Path,
        legacy_bytes: &[u8],
    ) -> (
        MigrationParticipantRegistry,
        MigrationPlanDraftV1,
        (u64, String, String),
    ) {
        basic_migration_draft_with_limits(path, legacy_bytes, StoreLimits::default())
    }

    fn basic_migration_draft_with_limits(
        path: &Path,
        legacy_bytes: &[u8],
        code_source_limits: StoreLimits,
    ) -> (
        MigrationParticipantRegistry,
        MigrationPlanDraftV1,
        (u64, String, String),
    ) {
        let root = path.parent().unwrap();
        let transaction_id = ProjectCatalogTransactionId::mint();
        let publisher_source = root.join("publisher-refs.json");
        let mut registry = MigrationParticipantRegistry::new(
            path,
            root.join("code-source"),
            publisher_source,
            code_source_limits,
        )
        .unwrap();
        let checkout_root = root.join("checkout");
        registry
            .register_checkout_identity("checkout-observation-1".into(), checkout_root.clone())
            .unwrap();
        let mut catalog = CatalogSnapshotV2::empty(1).unwrap();
        catalog.origin = CatalogOriginV2::MigratedV1 {
            transaction_id: transaction_id.clone(),
        };
        let project_id = ProjectId::parse("checkout-project").unwrap();
        catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::LegacyLocal,
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Checkout project".into(),
                created_at: "2026-07-23T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        let mut attachments = AttachmentSnapshotV1::empty(1).unwrap();
        let attachment_id = AttachmentId::parse("att_77777777777777777777777777777777").unwrap();
        let checkout_dir = checkout_root.to_string_lossy().into_owned();
        attachments.attachments.insert(
            attachment_id.clone(),
            CheckoutAttachment {
                attachment_id,
                project_id,
                checkout_id: "66666666666666666666666666666666".into(),
                checkout_dir: checkout_dir.clone(),
                checkout_project_dir: checkout_dir,
                project_root_relpath: ".".into(),
                kind: AttachmentKind::Base,
                validated_scope: None,
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities {
                    local_code_source: true,
                    ..AttachmentCapabilities::default()
                },
                status: AttachmentStatus::Attached,
                attached_at: "2026-07-23T00:00:00Z".into(),
                detached_at: None,
            },
        );
        attachments.validate().unwrap();
        let effective_bytes =
            bbox_code_source_store::encode_migration_effective_source_manifest_v1(
                &MigrationEffectiveSourceManifestV1 {
                    version: 1,
                    selections: Vec::new(),
                },
            )
            .unwrap();
        let expected = state_fingerprint(
            &PreparedPair::new(catalog.clone(), attachments.clone())
                .unwrap()
                .into_state(),
        );
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_catalog_scopes(&catalog),
        )
        .unwrap();
        let inventory_sha256 = Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        let quarantine_authority =
            crate::project_catalog_inventory::tests::validated_quarantine_bindings_fixture(
                transaction_id.clone(),
                BTreeMap::new(),
                BTreeSet::new(),
            );
        let plan_hash = Sha256Hex::parse(quarantine_authority.plan_hash().to_string()).unwrap();
        let draft = MigrationPlanDraftV1 {
            transaction_id,
            plan_hash: plan_hash.clone(),
            report_artifact_sha256: Sha256Hex::digest(b"reviewed migration report"),
            resolution_artifact_sha256: Sha256Hex::digest(b"reviewed migration resolution"),
            legacy_project_source: MigrationLegacyProjectSourceDraftV1::Present(
                legacy_bytes.to_vec(),
            ),
            publisher_ref_source: MigrationPublisherSourceDraftV1::Missing,
            inventory_sha256: inventory_sha256.clone(),
            code_source_inventory_sha256: inventory_sha256,
            catalog,
            attachments,
            participants: vec![MigrationParticipantDraftV1::new(
                ParticipantRoleV1::EffectiveSourceManifest,
                None,
                Some(effective_bytes),
            )],
            immutable_assets: vec![MigrationImmutableAssetDraftV1::new(
                ImmutableAssetRoleV1::LegacyProjectStoreBackup,
                legacy_bytes.to_vec(),
            )],
            code_source_snapshot: MigrationCodeSourceSnapshotDraftV1 {
                legacy_inventory,
                activations: Vec::new(),
                generations: Vec::new(),
            },
            quarantine_authority,
            publisher_pins: Vec::new(),
            publisher_dispositions: Vec::new(),
            checkout_identity_actions: vec![MigrationCheckoutIdentityActionDraftV1::new(
                "checkout-observation-1".into(),
                "66666666666666666666666666666666".into(),
            )],
        };
        (registry, draft, expected)
    }

    fn migration_fault_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        ValidatedMigrationPlanV1,
        (u64, String, String),
        Vec<u8>,
    ) {
        let (directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n".to_vec();
        fs::write(&path, &legacy_bytes).unwrap();
        let (registry, draft, expected) = basic_migration_draft(&path, &legacy_bytes);
        let plan = validate_migration_plan(&path, registry, draft).unwrap();
        (directory, path, plan, expected, legacy_bytes)
    }

    fn missing_source_migration_fault_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        ValidatedMigrationPlanV1,
        (u64, String, String),
    ) {
        let (directory, path) = projects_path();
        let (registry, mut draft, expected) = basic_migration_draft(&path, b"");
        draft.legacy_project_source = MigrationLegacyProjectSourceDraftV1::Missing;
        draft
            .immutable_assets
            .retain(|asset| asset.role != ImmutableAssetRoleV1::LegacyProjectStoreBackup);
        let plan = validate_migration_plan(&path, registry, draft).unwrap();
        (directory, path, plan, expected)
    }

    fn publisher_seed_migration_draft(
        path: &Path,
        legacy_bytes: &[u8],
    ) -> (
        MigrationParticipantRegistry,
        MigrationPlanDraftV1,
        ParticipantRoleV1,
        ImmutableAssetRoleV1,
    ) {
        use crate::accepted_publication_store::{
            AcceptedPublicationBuildInputV1, prepare_accepted_publication_v1,
        };

        let (mut registry, mut draft, _) = basic_migration_draft(path, legacy_bytes);
        let project_id = ProjectId::parse("published-project").unwrap();
        let attachment_id = AttachmentId::parse("att_aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").unwrap();
        let scope = PublishedScope::try_new("published-repo", ".").unwrap();
        let full_ref = FullPublisherRef::parse("refs/heads/main").unwrap();
        let accepted_commit = GitObjectId::parse("a".repeat(40)).unwrap();
        draft.catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Published project".into(),
                created_at: "2026-07-23T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        let published_checkout_root = path.parent().unwrap().join("published-project");
        let published_checkout_dir = published_checkout_root.to_string_lossy().into_owned();
        draft.attachments.attachments.insert(
            attachment_id.clone(),
            CheckoutAttachment {
                attachment_id: attachment_id.clone(),
                project_id: project_id.clone(),
                checkout_id: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
                checkout_dir: published_checkout_dir.clone(),
                checkout_project_dir: published_checkout_dir,
                project_root_relpath: ".".into(),
                kind: AttachmentKind::Base,
                validated_scope: Some(scope.clone()),
                computed_repo_hint: None,
                branch_ref: None,
                capabilities: AttachmentCapabilities {
                    local_code_source: true,
                    ..AttachmentCapabilities::default()
                },
                status: AttachmentStatus::Attached,
                attached_at: "2026-07-23T00:00:00Z".into(),
                detached_at: None,
            },
        );
        registry
            .register_checkout_identity(
                "publisher-checkout-observation-1".into(),
                published_checkout_root,
            )
            .unwrap();
        draft
            .checkout_identity_actions
            .push(MigrationCheckoutIdentityActionDraftV1::new(
                "publisher-checkout-observation-1".into(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".into(),
            ));
        let prepared = prepare_accepted_publication_v1(
            AcceptedPublicationBuildInputV1 {
                project_id: project_id.clone(),
                attachment_id: attachment_id.clone(),
                scope: scope.clone(),
                full_ref: full_ref.clone(),
                accepted_commit: accepted_commit.clone(),
                knowledge: Vec::new(),
                gaps: Vec::new(),
                prior_pointer: None,
            },
            &AcceptedPublicationLimits::default(),
        )
        .unwrap();
        let pointer_role = ParticipantRoleV1::AcceptedPublicationPointer {
            project_id: project_id.clone(),
        };
        draft.participants.push(MigrationParticipantDraftV1::new(
            pointer_role.clone(),
            None,
            Some(prepared.pointer_bytes.clone()),
        ));
        let generation_role = ImmutableAssetRoleV1::AcceptedPublicationGeneration {
            project_id: project_id.clone(),
            generation_id: prepared.generation_id.clone(),
        };
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::new(
                generation_role.clone(),
                prepared.generation_bytes.clone(),
            ));
        let generation_sha256 =
            Sha256Hex::parse(prepared.generation_hash.as_str().to_string()).unwrap();
        let pointer_sha256 = Sha256Hex::parse(prepared.pointer_hash.as_str().to_string()).unwrap();
        let publisher_bytes = serde_json::to_vec_pretty(&serde_json::json!({
            "version": 1,
            "refs": [{
                "scope": scope.clone(),
                "branch_ref": full_ref.as_str(),
            }],
        }))
        .unwrap();
        fs::write(&registry.legacy_publisher_ref_source, &publisher_bytes).unwrap();
        draft.publisher_ref_source =
            MigrationPublisherSourceDraftV1::Present(publisher_bytes.clone());
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::new(
                ImmutableAssetRoleV1::LegacyPublisherRefBackup,
                publisher_bytes,
            ));
        draft.publisher_pins.push(PublisherPinEvidenceV1 {
            observation_id: "publisher-observation-1".into(),
            project_id: project_id.clone(),
            expected_scope: scope.clone(),
            full_ref: full_ref.clone(),
            candidate_attachment_ids: BTreeSet::from([attachment_id.clone()]),
            resolved_commit: Some(accepted_commit.clone()),
            resolved_scope: Some(scope.clone()),
            source_observation_ids: BTreeSet::from(["publisher-source-row-1".into()]),
        });
        draft
            .publisher_dispositions
            .push(PublisherDispositionEvidenceV1::SeedG1 {
                observation_id: "publisher-observation-1".into(),
                project_id,
                attachment_id,
                expected_scope: scope,
                full_ref,
                accepted_commit,
                generation_id: prepared.generation_id,
                generation_sha256,
                pointer_sha256,
            });
        (registry, draft, pointer_role, generation_role)
    }

    fn refresh_test_quarantine_authority(
        registry: &MigrationParticipantRegistry,
        draft: &mut MigrationPlanDraftV1,
        bindings: BTreeSet<(ProjectId, crate::project_catalog_inventory::Sha256ValueV1)>,
    ) {
        let generation_owners = draft
            .code_source_snapshot
            .generations
            .iter()
            .map(|generation| {
                (
                    crate::project_catalog_inventory::Sha256ValueV1::parse(
                        generation.generation_id.to_string(),
                    )
                    .unwrap(),
                    generation.project_id.clone(),
                )
            })
            .collect();
        draft.quarantine_authority =
            crate::project_catalog_inventory::tests::validated_quarantine_bindings_fixture(
                draft.transaction_id.clone(),
                generation_owners,
                bindings,
            );
        draft.plan_hash =
            Sha256Hex::parse(draft.quarantine_authority.plan_hash().to_string()).unwrap();
        for participant in &mut draft.participants {
            let ParticipantRoleV1::CollisionRetirement { project_id } = &participant.role else {
                continue;
            };
            if participant.expected_old_sha256.is_some() {
                let target = registry
                    .code_source_paths
                    .collision_retirement_pending(project_id);
                let mut previous =
                    decode_collision_retirement_pending_for_migration(&fs::read(&target).unwrap())
                        .unwrap();
                for entry in previous.entries.values_mut() {
                    entry.plan_hash = draft.plan_hash.to_string();
                }
                let previous =
                    bbox_code_source_store::encode_collision_retirement_pending_for_migration(
                        &previous,
                    )
                    .unwrap();
                fs::write(&target, &previous).unwrap();
                participant.expected_old_sha256 = Some(Sha256Hex::digest(&previous));
            }
            let bytes = participant
                .post_image
                .as_deref()
                .expect("collision retirement has a post-image");
            let mut retirement = decode_collision_retirement_pending_for_migration(bytes).unwrap();
            for entry in retirement.entries.values_mut() {
                entry.plan_hash = draft.plan_hash.to_string();
            }
            participant.post_image = Some(
                bbox_code_source_store::encode_collision_retirement_pending_for_migration(
                    &retirement,
                )
                .unwrap(),
            );
        }
        let inventory_scopes = migration_inventory_scopes(
            &draft.catalog,
            draft
                .participants
                .iter()
                .map(|participant| (&participant.role, participant.post_image.as_deref())),
        )
        .unwrap();
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &inventory_scopes,
        )
        .unwrap();
        let code_source_inventory_sha256 =
            Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        draft.inventory_sha256 = code_source_inventory_sha256.clone();
        draft.code_source_inventory_sha256 = code_source_inventory_sha256;
        draft.code_source_snapshot.legacy_inventory = legacy_inventory;
        for participant in &mut draft.participants {
            if !matches!(
                participant.role,
                ParticipantRoleV1::CollisionRetirement { .. }
            ) || participant.expected_old_sha256.is_some()
            {
                continue;
            }
            let bytes = participant
                .post_image
                .as_deref()
                .expect("collision retirement has a post-image");
            let mut retirement = decode_collision_retirement_pending_for_migration(bytes).unwrap();
            for entry in retirement.entries.values_mut() {
                entry.inventory_hash = draft.inventory_sha256.to_string();
            }
            participant.post_image = Some(
                bbox_code_source_store::encode_collision_retirement_pending_for_migration(
                    &retirement,
                )
                .unwrap(),
            );
        }
    }

    fn add_named_collision_retirement_to_draft(
        registry: &MigrationParticipantRegistry,
        draft: &mut MigrationPlanDraftV1,
        project_name: &str,
        repo_name: &str,
        producer_name: &str,
        observation_prefix: &str,
        seed_legacy_lifecycle: bool,
    ) -> (ImmutableAssetRoleV1, PathBuf, Vec<u8>) {
        let project_id = ProjectId::parse(project_name).unwrap();
        let former_scope = PublishedScope::try_new(repo_name, ".").unwrap();
        let entries = Vec::<bbox_code_source::ManifestEntry>::new();
        let head_commit = "d".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: former_scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let producer_id = producer_name;
        let generation_id =
            Sha256Hex::parse(bbox_code_source::generation_id(producer_id, &descriptor)).unwrap();
        let selector = historical_selector(project_id.as_str(), generation_id.as_str());
        let manifest_bytes = Vec::new();
        let manifest_sha256 = Sha256Hex::digest(&manifest_bytes);
        draft.catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::LegacyLocal,
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Collision project".into(),
                created_at: "2026-07-23T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        let old_stored = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: generation_id.to_string(),
            producer_id: producer_id.into(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            state: bbox_code_source::GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let activation = bbox_code_source_store::ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation_id.to_string(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            diagnostic: None,
        };
        let activation_bytes = serde_json::to_vec_pretty(&activation).unwrap();
        let stored_bytes = serde_json::to_vec_pretty(&old_stored).unwrap();
        decode_activation_v1_for_migration(&activation_bytes).unwrap();
        decode_stored_generation_v1_for_migration(&stored_bytes).unwrap();
        let activation_target = registry.code_source_paths.activation(&project_id);
        fs::create_dir_all(activation_target.parent().unwrap()).unwrap();
        fs::write(&activation_target, &activation_bytes).unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::Activation {
                project_id: project_id.clone(),
            },
            Some(Sha256Hex::digest(&activation_bytes)),
            None,
        ));
        let stored_role = ParticipantRoleV1::StoredGenerationMetadata {
            project_id: project_id.clone(),
            published_scope: former_scope.clone(),
            generation_id: generation_id.clone(),
        };
        let stored_target = registry
            .code_source_paths
            .generation_metadata(&former_scope, generation_id.as_str())
            .unwrap();
        fs::create_dir_all(stored_target.parent().unwrap()).unwrap();
        fs::write(&stored_target, &stored_bytes).unwrap();
        let new_stored = bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
            old_stored,
            former_scope.clone(),
        )
        .unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            stored_role,
            Some(Sha256Hex::digest(&stored_bytes)),
            Some(
                bbox_code_source_store::encode_stored_generation_v2_for_migration(&new_stored)
                    .unwrap(),
            ),
        ));
        let manifest_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope: former_scope.clone(),
            generation_id: generation_id.clone(),
        };
        let manifest_name =
            immutable_target_name(&draft.transaction_id, &manifest_role, &manifest_sha256).unwrap();
        let manifest_target = registry.immutable_target(&manifest_role, &manifest_name);
        fs::create_dir_all(manifest_target.parent().unwrap()).unwrap();
        fs::write(&manifest_target, &manifest_bytes).unwrap();
        let retained_descriptor = bbox_code_source::GenerationDescriptor {
            head_commit: "e".repeat(40),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&"e".repeat(40), &entries),
            ..descriptor.clone()
        };
        let retained_producer = format!("{producer_name}-retained");
        let retained_generation_id = Sha256Hex::parse(bbox_code_source::generation_id(
            &retained_producer,
            &retained_descriptor,
        ))
        .unwrap();
        let retained_stored = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: retained_generation_id.to_string(),
            producer_id: retained_producer,
            ordinal: 0,
            descriptor: retained_descriptor,
            state: bbox_code_source::GenerationState::Superseded,
            diagnostic: None,
            created_unix_secs: 0,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let retained_bytes = serde_json::to_vec_pretty(&retained_stored).unwrap();
        let retained_target = registry
            .code_source_paths
            .generation_metadata(&former_scope, retained_generation_id.as_str())
            .unwrap();
        fs::create_dir_all(retained_target.parent().unwrap()).unwrap();
        fs::write(&retained_target, &retained_bytes).unwrap();
        fs::write(
            registry
                .code_source_paths
                .generation_manifest(&former_scope, retained_generation_id.as_str())
                .unwrap(),
            &manifest_bytes,
        )
        .unwrap();
        let retained_v2 = bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
            retained_stored,
            former_scope.clone(),
        )
        .unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id: project_id.clone(),
                published_scope: former_scope.clone(),
                generation_id: retained_generation_id.clone(),
            },
            Some(Sha256Hex::digest(&retained_bytes)),
            Some(
                bbox_code_source_store::encode_stored_generation_v2_for_migration(&retained_v2)
                    .unwrap(),
            ),
        ));
        let retained_manifest_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope: former_scope.clone(),
            generation_id: retained_generation_id.clone(),
        };
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::pinned_existing(
                retained_manifest_role,
                manifest_sha256.clone(),
            ));
        let legacy_retirement = bbox_code_source_store::CollisionRetirementLifecycleV1 {
            version: 1,
            project_id: project_id.clone(),
            entries: BTreeMap::from([
                (
                    generation_id.to_string(),
                    bbox_code_source_store::CollisionRetirementEntryV1 {
                        state:
                            bbox_code_source_store::CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: former_scope.clone(),
                        selector_evidence: bbox_code_source_store::
                            CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
                                selector.clone(),
                            ),
                        snapshot_id: activation.snapshot_id.clone(),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "0".repeat(64),
                        plan_hash: draft.plan_hash.to_string(),
                    },
                ),
                (
                    retained_generation_id.to_string(),
                    bbox_code_source_store::CollisionRetirementEntryV1 {
                        state:
                            bbox_code_source_store::CollisionRetirementLifecycleStateV1::Pending,
                        former_scope: former_scope.clone(),
                        selector_evidence: bbox_code_source_store::
                            CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                        snapshot_id: format!("collected-{}", "f".repeat(32)),
                        manifest_sha256: descriptor.manifest_sha256.clone(),
                        inventory_hash: "0".repeat(64),
                        plan_hash: draft.plan_hash.to_string(),
                    },
                ),
            ]),
        };
        let legacy_retirement_bytes =
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(
                &legacy_retirement,
            )
            .unwrap();
        let legacy_retirement_target = registry
            .code_source_paths
            .collision_retirement_pending(&project_id);
        if seed_legacy_lifecycle {
            fs::create_dir_all(legacy_retirement_target.parent().unwrap()).unwrap();
            fs::write(&legacy_retirement_target, &legacy_retirement_bytes).unwrap();
        }
        let mut inventory_scopes = published_catalog_scopes(&draft.catalog);
        inventory_scopes.insert(former_scope.clone());
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &inventory_scopes,
        )
        .unwrap();
        assert!(
            legacy_inventory
                .generations
                .iter()
                .any(|generation| { generation.generation_id == retained_generation_id.as_str() })
        );
        draft.inventory_sha256 =
            Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        let retirement = if seed_legacy_lifecycle {
            legacy_retirement.clone()
        } else {
            bbox_code_source_store::CollisionRetirementLifecycleV1 {
                version: 1,
                project_id: project_id.clone(),
                entries: BTreeMap::from([
                    (
                        generation_id.to_string(),
                        bbox_code_source_store::CollisionRetirementEntryV1 {
                            state:
                                bbox_code_source_store::CollisionRetirementLifecycleStateV1::Pending,
                            former_scope: former_scope.clone(),
                            selector_evidence: bbox_code_source_store::
                                CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector),
                            snapshot_id: activation.snapshot_id,
                            manifest_sha256: descriptor.manifest_sha256.clone(),
                            inventory_hash: draft.inventory_sha256.to_string(),
                            plan_hash: draft.plan_hash.to_string(),
                        },
                    ),
                    (
                        retained_generation_id.to_string(),
                        bbox_code_source_store::CollisionRetirementEntryV1 {
                            state:
                                bbox_code_source_store::CollisionRetirementLifecycleStateV1::Pending,
                            former_scope: former_scope.clone(),
                            selector_evidence: bbox_code_source_store::
                                CollisionRetirementSelectorEvidenceV1::NoDurableSelector,
                            snapshot_id: format!("collected-{}", "f".repeat(32)),
                            manifest_sha256: descriptor.manifest_sha256.clone(),
                            inventory_hash: draft.inventory_sha256.to_string(),
                            plan_hash: draft.plan_hash.to_string(),
                        },
                    ),
                ]),
            }
        };
        let retirement_bytes =
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&retirement)
                .unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::CollisionRetirement {
                project_id: project_id.clone(),
            },
            seed_legacy_lifecycle.then(|| Sha256Hex::digest(&legacy_retirement_bytes)),
            Some(retirement_bytes),
        ));
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::pinned_existing(
                manifest_role.clone(),
                manifest_sha256,
            ));
        draft
            .code_source_snapshot
            .activations
            .push(MigrationCodeSourceActivationDraftV1 {
                observation_id: format!("{observation_prefix}-activation"),
                project_id: project_id.clone(),
                disposition: MigrationCodeSourceDispositionV1::QuarantinedCollision,
            });
        let mut quarantine_bindings = draft.quarantine_authority.bindings().clone();
        quarantine_bindings.extend([
            (
                project_id.clone(),
                crate::project_catalog_inventory::Sha256ValueV1::parse(generation_id.to_string())
                    .unwrap(),
            ),
            (
                project_id.clone(),
                crate::project_catalog_inventory::Sha256ValueV1::parse(
                    retained_generation_id.to_string(),
                )
                .unwrap(),
            ),
        ]);
        draft
            .code_source_snapshot
            .generations
            .push(MigrationCodeSourceGenerationDraftV1 {
                observation_id: format!("{observation_prefix}-generation"),
                project_id: project_id.clone(),
                generation_id,
                disposition: MigrationCodeSourceDispositionV1::QuarantinedCollision,
            });
        draft
            .code_source_snapshot
            .generations
            .push(MigrationCodeSourceGenerationDraftV1 {
                observation_id: format!("{observation_prefix}-retained-generation"),
                project_id,
                generation_id: retained_generation_id,
                disposition: MigrationCodeSourceDispositionV1::QuarantinedCollision,
            });
        draft.code_source_snapshot.legacy_inventory = legacy_inventory;
        refresh_test_quarantine_authority(registry, draft, quarantine_bindings);
        (manifest_role, manifest_target, manifest_bytes)
    }

    fn add_collision_retirement_to_draft(
        registry: &MigrationParticipantRegistry,
        draft: &mut MigrationPlanDraftV1,
    ) -> (ImmutableAssetRoleV1, PathBuf, Vec<u8>) {
        add_named_collision_retirement_to_draft(
            registry,
            draft,
            "collision-project",
            "collision-repo",
            "collision-producer",
            "collision-observation-1",
            true,
        )
    }

    fn make_collision_retained_only(
        registry: &MigrationParticipantRegistry,
        draft: &mut MigrationPlanDraftV1,
        project_id: &ProjectId,
    ) -> Sha256Hex {
        let collision_role = ParticipantRoleV1::CollisionRetirement {
            project_id: project_id.clone(),
        };
        let collision = draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == collision_role)
            .unwrap();
        assert!(collision.expected_old_sha256.is_none());
        let mut lifecycle = decode_collision_retirement_pending_for_migration(
            collision.post_image.as_deref().unwrap(),
        )
        .unwrap();
        let exact_generation_id = lifecycle
            .entries
            .iter()
            .find_map(|(generation_id, entry)| {
                entry
                    .exact_selector()
                    .is_some()
                    .then_some(Sha256Hex::parse(generation_id.clone()).unwrap())
            })
            .unwrap();
        lifecycle.entries.remove(exact_generation_id.as_str());
        let retained_generation_id =
            Sha256Hex::parse(lifecycle.entries.keys().next().unwrap().clone()).unwrap();
        collision.post_image = Some(
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&lifecycle)
                .unwrap(),
        );

        draft.participants.retain(|participant| {
            !matches!(
                &participant.role,
                ParticipantRoleV1::Activation {
                    project_id: activation_project,
                } if activation_project == project_id
            ) && !matches!(
                &participant.role,
                ParticipantRoleV1::StoredGenerationMetadata {
                    project_id: stored_project,
                    generation_id,
                    ..
                } if stored_project == project_id && generation_id == &exact_generation_id
            )
        });
        draft.immutable_assets.retain(|asset| {
            !matches!(
                &asset.role,
                ImmutableAssetRoleV1::CollectedGenerationManifest { generation_id, .. }
                    if generation_id == &exact_generation_id
            )
        });
        draft
            .code_source_snapshot
            .activations
            .retain(|activation| &activation.project_id != project_id);
        draft.code_source_snapshot.generations.retain(|generation| {
            &generation.project_id != project_id || generation.generation_id != exact_generation_id
        });

        let activation_path = registry.code_source_paths.activation(project_id);
        if activation_path.exists() {
            fs::remove_file(activation_path).unwrap();
        }
        let former_scope = lifecycle
            .entries
            .values()
            .next()
            .unwrap()
            .former_scope
            .clone();
        let exact_metadata = registry
            .code_source_paths
            .generation_metadata(&former_scope, exact_generation_id.as_str())
            .unwrap();
        if let Some(directory) = exact_metadata.parent()
            && directory.exists()
        {
            fs::remove_dir_all(directory).unwrap();
        }

        let inventory_scopes = migration_inventory_scopes(
            &draft.catalog,
            draft
                .participants
                .iter()
                .map(|participant| (&participant.role, participant.post_image.as_deref())),
        )
        .unwrap();
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &inventory_scopes,
        )
        .unwrap();
        assert!(
            legacy_inventory
                .generations
                .iter()
                .any(|generation| generation.generation_id == retained_generation_id.as_str()),
            "retained-only fixture lost its retained generation"
        );
        draft.inventory_sha256 =
            Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        draft.code_source_snapshot.legacy_inventory = legacy_inventory;
        let collision = draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == collision_role)
            .unwrap();
        let mut lifecycle = decode_collision_retirement_pending_for_migration(
            collision.post_image.as_deref().unwrap(),
        )
        .unwrap();
        for entry in lifecycle.entries.values_mut() {
            entry.inventory_hash = draft.inventory_sha256.to_string();
        }
        collision.post_image = Some(
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&lifecycle)
                .unwrap(),
        );
        refresh_test_quarantine_authority(
            registry,
            draft,
            BTreeSet::from([(
                project_id.clone(),
                crate::project_catalog_inventory::Sha256ValueV1::parse(
                    retained_generation_id.to_string(),
                )
                .unwrap(),
            )]),
        );
        retained_generation_id
    }

    fn revalidate_plan_cross_roles(
        plan: &ValidatedMigrationPlanV1,
    ) -> ProjectCatalogStoreResult<()> {
        let catalog = decode_catalog_snapshot(
            plan.post_images[&ParticipantRoleV1::Catalog]
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let attachments = decode_attachment_snapshot(
            plan.post_images[&ParticipantRoleV1::Attachments]
                .as_deref()
                .unwrap(),
        )
        .unwrap();
        let immutable_assets = plan
            .journal
            .immutable_assets
            .iter()
            .map(|asset| MigrationImmutableAssetEvidenceV1 {
                role: asset.role.clone(),
                mode: asset.mode,
                sha256: asset.sha256.clone(),
                validated_name: asset.validated_name.clone(),
            })
            .collect::<Vec<_>>();
        validate_new_side_cross_roles(
            &catalog,
            &attachments,
            &plan.post_images,
            &immutable_assets,
            &plan.immutable_asset_bytes,
            &plan.journal.publisher_pins,
            &plan.journal.publisher_dispositions,
            "error.project_catalog_invalid_migration_plan",
        )
    }

    fn assert_existing_collision_lifecycle_mutation_refused(
        mutate: impl FnOnce(&mut bbox_code_source_store::CollisionRetirementLifecycleV1),
    ) {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_collision_retirement_to_draft(&registry, &mut draft);
        let participant = draft
            .participants
            .iter_mut()
            .find(|participant| {
                matches!(
                    participant.role,
                    ParticipantRoleV1::CollisionRetirement { .. }
                )
            })
            .unwrap();
        assert!(participant.expected_old_sha256.is_some());
        let mut lifecycle = decode_collision_retirement_pending_for_migration(
            participant.post_image.as_deref().unwrap(),
        )
        .unwrap();
        mutate(&mut lifecycle);
        participant.post_image = Some(
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&lifecycle)
                .unwrap(),
        );

        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    fn add_retained_generation_to_draft(
        registry: &MigrationParticipantRegistry,
        draft: &mut MigrationPlanDraftV1,
    ) {
        let project_id = ProjectId::parse("retained-project").unwrap();
        let scope = PublishedScope::try_new("retained-repo", ".").unwrap();
        draft.catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Retained project".into(),
                created_at: "2026-07-23T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        let entries = Vec::<bbox_code_source::ManifestEntry>::new();
        let head_commit = "f".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let producer_id = "retained-producer";
        let generation_id =
            Sha256Hex::parse(bbox_code_source::generation_id(producer_id, &descriptor)).unwrap();
        let old_stored = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: generation_id.to_string(),
            producer_id: producer_id.into(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            state: bbox_code_source::GenerationState::Superseded,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let new_stored = bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
            old_stored.clone(),
            scope.clone(),
        )
        .unwrap();
        let old_stored_bytes = serde_json::to_vec_pretty(&old_stored).unwrap();
        let stored_target = registry
            .code_source_paths
            .generation_metadata(&scope, generation_id.as_str())
            .unwrap();
        fs::create_dir_all(stored_target.parent().unwrap()).unwrap();
        fs::write(&stored_target, &old_stored_bytes).unwrap();
        let manifest_bytes = Vec::new();
        let manifest_sha256 = Sha256Hex::digest(&manifest_bytes);
        let manifest_target = registry
            .code_source_paths
            .generation_manifest(&scope, generation_id.as_str())
            .unwrap();
        fs::write(&manifest_target, &manifest_bytes).unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id: project_id.clone(),
                published_scope: scope.clone(),
                generation_id: generation_id.clone(),
            },
            Some(Sha256Hex::digest(&old_stored_bytes)),
            Some(
                bbox_code_source_store::encode_stored_generation_v2_for_migration(&new_stored)
                    .unwrap(),
            ),
        ));
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::pinned_existing(
                ImmutableAssetRoleV1::CollectedGenerationManifest {
                    published_scope: scope.clone(),
                    generation_id: generation_id.clone(),
                },
                manifest_sha256.clone(),
            ));
        draft
            .code_source_snapshot
            .generations
            .push(MigrationCodeSourceGenerationDraftV1 {
                observation_id: "retained-generation-observation-1".into(),
                project_id,
                generation_id,
                disposition: MigrationCodeSourceDispositionV1::SurvivingRetained,
            });
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_catalog_scopes(&draft.catalog),
        )
        .unwrap();
        draft.inventory_sha256 =
            Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        draft.code_source_snapshot.legacy_inventory = legacy_inventory;
        let quarantine_bindings = draft.quarantine_authority.bindings().clone();
        refresh_test_quarantine_authority(registry, draft, quarantine_bindings);
    }

    fn extended_migration_fault_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        ValidatedMigrationPlanV1,
        PathBuf,
        Vec<u8>,
    ) {
        let (directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n".to_vec();
        fs::write(&path, &legacy_bytes).unwrap();
        let (registry, mut draft, _, _) = publisher_seed_migration_draft(&path, &legacy_bytes);
        let (_, manifest_target, manifest_bytes) =
            add_collision_retirement_to_draft(&registry, &mut draft);
        add_retained_generation_to_draft(&registry, &mut draft);
        let plan = validate_migration_plan(&path, registry, draft).unwrap();
        (directory, path, plan, manifest_target, manifest_bytes)
    }

    #[test]
    fn collision_generation_ownership_rejects_a_two_project_swap() {
        let (_directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy_bytes).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            "collision-project-a",
            "collision-repo-a",
            "collision-producer-a",
            "collision-observation-a",
            true,
        );
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            "collision-project-b",
            "collision-repo-b",
            "collision-producer-b",
            "collision-observation-b",
            true,
        );
        let project_a = ProjectId::parse("collision-project-a").unwrap();
        let project_b = ProjectId::parse("collision-project-b").unwrap();
        assert_eq!(
            draft
                .code_source_snapshot
                .generations
                .iter()
                .filter(|row| row.project_id == project_a)
                .count(),
            2
        );
        assert_eq!(
            draft
                .code_source_snapshot
                .generations
                .iter()
                .filter(|row| row.project_id == project_b)
                .count(),
            2
        );
        assert!(validate_migration_plan(&path, registry.clone(), draft.clone()).is_ok());

        for generation in &mut draft.code_source_snapshot.generations {
            generation.project_id = if generation.project_id == project_a {
                project_b.clone()
            } else if generation.project_id == project_b {
                project_a.clone()
            } else {
                generation.project_id.clone()
            };
        }
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    fn activation_migration_draft(
        path: &Path,
        legacy_bytes: &[u8],
    ) -> (
        MigrationParticipantRegistry,
        MigrationPlanDraftV1,
        ParticipantRoleV1,
        ParticipantRoleV1,
    ) {
        let (registry, mut draft, _) = basic_migration_draft(path, legacy_bytes);
        let project_id = ProjectId::parse("active-project").unwrap();
        let scope = PublishedScope::try_new("active-repo", ".").unwrap();
        draft.catalog.projects.insert(
            project_id.clone(),
            CorpusProject {
                project_id: project_id.clone(),
                scope: ProjectScope::Published(scope.clone()),
                operator_aliases: BTreeSet::new(),
                nominated_aliases: BTreeSet::new(),
                display_name: "Active project".into(),
                created_at: "2026-07-23T00:00:00Z".into(),
                registered_at_compat: None,
                repo_history: None,
                languages: BTreeSet::new(),
            },
        );
        let entries = Vec::<bbox_code_source::ManifestEntry>::new();
        let head_commit = "b".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let producer_id = "migration-producer";
        let generation_id = bbox_code_source::generation_id(producer_id, &descriptor);
        let selector = historical_selector(project_id.as_str(), &generation_id);
        let manifest_bytes = Vec::new();
        let manifest_sha256 = Sha256Hex::digest(&manifest_bytes);
        let old_generation = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: generation_id.clone(),
            producer_id: producer_id.into(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            state: bbox_code_source::GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let old_activation = bbox_code_source_store::ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation_id.clone(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "e".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            diagnostic: None,
        };
        let generation = bbox_code_source_store::StoredGenerationV2 {
            version: 2,
            generation_id: generation_id.clone(),
            producer_id: producer_id.into(),
            ordinal: 1,
            descriptor: descriptor.clone(),
            published_scope: scope.clone(),
            state: bbox_code_source::GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 1,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let activation = bbox_code_source_store::ActivationRecordV2 {
            version: 2,
            project_id: project_id.clone(),
            published_scope: scope.clone(),
            generation_id: generation_id.clone(),
            selector: selector.clone(),
            snapshot_id: old_activation.snapshot_id.clone(),
            document_count: 0,
            entity_inventory_sha256: "c".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        let activation_role = ParticipantRoleV1::Activation {
            project_id: project_id.clone(),
        };
        let generation_id = Sha256Hex::parse(generation_id).unwrap();
        let stored_role = ParticipantRoleV1::StoredGenerationMetadata {
            project_id: project_id.clone(),
            published_scope: scope.clone(),
            generation_id: generation_id.clone(),
        };
        let old_activation_bytes = serde_json::to_vec_pretty(&old_activation).unwrap();
        let old_stored_bytes = serde_json::to_vec_pretty(&old_generation).unwrap();
        let effective = MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections: vec![
                bbox_code_source_store::MigrationEffectiveSourceSelectionV1 {
                    project_id: project_id.clone(),
                    published_scope: scope.clone(),
                    generation_id: generation_id.to_string(),
                    selector: selector.clone(),
                },
            ],
        };
        let effective_bytes =
            bbox_code_source_store::encode_migration_effective_source_manifest_v1(&effective)
                .unwrap();
        let effective_participant = draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == ParticipantRoleV1::EffectiveSourceManifest)
            .unwrap();
        effective_participant.expected_old_sha256 = None;
        effective_participant.post_image = Some(effective_bytes.clone());
        let activation_target = registry.code_source_paths.activation(&project_id);
        fs::create_dir_all(activation_target.parent().unwrap()).unwrap();
        fs::write(&activation_target, &old_activation_bytes).unwrap();
        let stored_target = registry
            .code_source_paths
            .generation_metadata(&scope, generation_id.as_str())
            .unwrap();
        fs::create_dir_all(stored_target.parent().unwrap()).unwrap();
        fs::write(&stored_target, &old_stored_bytes).unwrap();
        let manifest_target = registry
            .code_source_paths
            .generation_manifest(&scope, generation_id.as_str())
            .unwrap();
        fs::write(&manifest_target, &manifest_bytes).unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            activation_role.clone(),
            Some(Sha256Hex::digest(&old_activation_bytes)),
            Some(bbox_code_source_store::encode_activation_v2_for_migration(&activation).unwrap()),
        ));
        draft.participants.push(MigrationParticipantDraftV1::new(
            stored_role.clone(),
            Some(Sha256Hex::digest(&old_stored_bytes)),
            Some(
                bbox_code_source_store::encode_stored_generation_v2_for_migration(&generation)
                    .unwrap(),
            ),
        ));
        let manifest_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope: scope.clone(),
            generation_id: generation_id.clone(),
        };
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::pinned_existing(
                manifest_role,
                manifest_sha256.clone(),
            ));
        let retained_descriptor = bbox_code_source::GenerationDescriptor {
            head_commit: "c".repeat(40),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&"c".repeat(40), &entries),
            ..descriptor.clone()
        };
        let retained_producer = "migration-retained-producer";
        let retained_generation_id = Sha256Hex::parse(bbox_code_source::generation_id(
            retained_producer,
            &retained_descriptor,
        ))
        .unwrap();
        let retained_old = bbox_code_source_store::StoredGeneration {
            version: 1,
            generation_id: retained_generation_id.to_string(),
            producer_id: retained_producer.into(),
            ordinal: 0,
            descriptor: retained_descriptor,
            state: bbox_code_source::GenerationState::Superseded,
            diagnostic: None,
            created_unix_secs: 0,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("c".repeat(64)),
        };
        let retained_new = bbox_code_source_store::StoredGenerationV2::from_v1_for_migration(
            retained_old.clone(),
            scope.clone(),
        )
        .unwrap();
        let retained_old_bytes = serde_json::to_vec_pretty(&retained_old).unwrap();
        let retained_target = registry
            .code_source_paths
            .generation_metadata(&scope, retained_generation_id.as_str())
            .unwrap();
        fs::create_dir_all(retained_target.parent().unwrap()).unwrap();
        fs::write(&retained_target, &retained_old_bytes).unwrap();
        fs::write(
            registry
                .code_source_paths
                .generation_manifest(&scope, retained_generation_id.as_str())
                .unwrap(),
            &manifest_bytes,
        )
        .unwrap();
        draft.participants.push(MigrationParticipantDraftV1::new(
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id: project_id.clone(),
                published_scope: scope.clone(),
                generation_id: retained_generation_id.clone(),
            },
            Some(Sha256Hex::digest(&retained_old_bytes)),
            Some(
                bbox_code_source_store::encode_stored_generation_v2_for_migration(&retained_new)
                    .unwrap(),
            ),
        ));
        draft
            .immutable_assets
            .push(MigrationImmutableAssetDraftV1::pinned_existing(
                ImmutableAssetRoleV1::CollectedGenerationManifest {
                    published_scope: scope.clone(),
                    generation_id: retained_generation_id.clone(),
                },
                manifest_sha256,
            ));
        let legacy_inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_catalog_scopes(&draft.catalog),
        )
        .unwrap();
        let code_source_inventory_sha256 =
            Sha256Hex::parse(legacy_inventory.canonical_sha256.clone()).unwrap();
        draft.inventory_sha256 = code_source_inventory_sha256.clone();
        draft.code_source_inventory_sha256 = code_source_inventory_sha256;
        draft.code_source_snapshot = MigrationCodeSourceSnapshotDraftV1 {
            legacy_inventory,
            activations: vec![MigrationCodeSourceActivationDraftV1 {
                observation_id: "active-activation-observation-1".into(),
                project_id: project_id.clone(),
                disposition: MigrationCodeSourceDispositionV1::SurvivingActive,
            }],
            generations: vec![
                MigrationCodeSourceGenerationDraftV1 {
                    observation_id: "active-generation-observation-1".into(),
                    project_id: project_id.clone(),
                    generation_id,
                    disposition: MigrationCodeSourceDispositionV1::SurvivingActive,
                },
                MigrationCodeSourceGenerationDraftV1 {
                    observation_id: "active-retained-generation-observation-1".into(),
                    project_id,
                    generation_id: retained_generation_id,
                    disposition: MigrationCodeSourceDispositionV1::SurvivingRetained,
                },
            ],
        };
        let quarantine_bindings = draft.quarantine_authority.bindings().clone();
        refresh_test_quarantine_authority(&registry, &mut draft, quarantine_bindings);
        (registry, draft, activation_role, stored_role)
    }

    fn active_migration_fault_fixture() -> (
        tempfile::TempDir,
        PathBuf,
        ValidatedMigrationPlanV1,
        ParticipantRoleV1,
        ParticipantRoleV1,
    ) {
        let (directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy_bytes).unwrap();
        let (registry, draft, activation_role, stored_role) =
            activation_migration_draft(&path, legacy_bytes);
        let plan = validate_migration_plan(&path, registry, draft).unwrap();
        (directory, path, plan, activation_role, stored_role)
    }

    #[test]
    fn migration_plan_rejects_an_omitted_owner_enumerated_generation() {
        let (_directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy_bytes).unwrap();
        let (registry, mut draft, _, _) = activation_migration_draft(&path, legacy_bytes);
        let published_scopes = published_catalog_scopes(&draft.catalog);
        let inventory_before = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_scopes,
        )
        .unwrap()
        .canonical_sha256;
        draft.code_source_snapshot.generations.clear();

        let error = validate_migration_plan(&path, registry.clone(), draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
        let inventory_after = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &registry.code_source_limits,
            &published_scopes,
        )
        .unwrap()
        .canonical_sha256;
        assert_eq!(inventory_after, inventory_before);
        assert_no_migration_transaction_outputs(&path);
    }

    #[test]
    fn derives_fixed_siblings_from_an_arbitrary_catalog_filename() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let paths = ProjectCatalogPaths::derive(&root.join("custom-projects.json")).unwrap();

        assert_eq!(paths.attachments, root.join("project-attachments.json"));
        assert_eq!(paths.journal, root.join("project-catalog-transaction.json"));
        assert_eq!(
            paths.migration_marker,
            root.join("project-catalog-migration.json")
        );
        assert_eq!(
            paths.migration_receipt,
            root.join("project-catalog-migration-receipt.json")
        );
        assert_eq!(
            paths.migration_assets_dir,
            root.join("project-catalog-migration-assets")
        );
        assert_eq!(paths.stage_dir, root.join("project-catalog-stage"));
        assert_eq!(paths.backup_dir, root.join("project-catalog-backups"));
        assert_eq!(paths.mutation_lock, root.join("custom-projects.json.lock"));
        assert!(ProjectCatalogPaths::derive(&root.join("project-attachments.json")).is_err());
        assert!(ProjectCatalogPaths::derive(Path::new("projects.json")).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn non_directory_parent_is_not_mapped_to_absent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let parent = root.join("not-a-directory");
        fs::write(&parent, b"file").unwrap();

        let error = RealCatalogStoreIo
            .read_regular_nofollow(&parent.join("child"), 128)
            .unwrap_err();

        assert_eq!(error.code(), "error.project_catalog_io");
    }

    #[test]
    fn cleanup_preserves_nonempty_evidence_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let evidence = root.join("retained-evidence");
        fs::create_dir(&evidence).unwrap();
        fs::write(evidence.join("retained.json"), b"retained").unwrap();

        RealCatalogStoreIo
            .remove_empty_dir_nofollow(&evidence)
            .unwrap();

        assert!(evidence.join("retained.json").is_file());
    }

    #[test]
    fn initialize_and_transact_publish_only_verified_pairs() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        assert_eq!(store.snapshot().unwrap().epoch(), 1);
        assert_eq!(
            store.snapshot().unwrap().catalog().version,
            CATALOG_VERSION_V2
        );
        assert_eq!(
            store.snapshot().unwrap().attachments().version,
            ATTACHMENT_VERSION_V1
        );

        let commit = store.transact(1, add_promoted_fixture).unwrap();
        assert_eq!(commit.epoch, 2);
        let reopened = ProjectCatalogStore::open_existing(path).unwrap();
        assert_eq!(reopened.snapshot().unwrap().epoch(), 2);
        assert_eq!(reopened.snapshot().unwrap().catalog().projects.len(), 1);
        assert_eq!(
            reopened
                .snapshot()
                .unwrap()
                .attachments()
                .scope_migration_proofs
                .len(),
            1
        );
        assert_eq!(
            reopened
                .snapshot()
                .unwrap()
                .attachments()
                .legacy_path_bindings
                .len(),
            1
        );
    }

    #[test]
    fn open_never_infers_an_empty_store_or_half_pair() {
        let (_directory, path) = projects_path();
        let error = ProjectCatalogStore::open_existing(path.clone()).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_not_initialized");

        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        fs::write(&path, encode_catalog_snapshot(&catalog).unwrap()).unwrap();
        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_incomplete_pair");
    }

    #[test]
    fn closure_cannot_change_owner_controlled_fields() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path).unwrap();
        let error = store
            .transact(1, |catalog, _| {
                catalog.epoch = 99;
                Ok(())
            })
            .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_owner_field_mutation");
        assert_eq!(store.snapshot().unwrap().epoch(), 1);
    }

    #[test]
    fn concurrent_stale_epoch_writers_have_exactly_one_winner() {
        let (_directory, path) = projects_path();
        let store = Arc::new(ProjectCatalogStore::initialize_empty(path).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for _ in 0..2 {
            let store = store.clone();
            let barrier = barrier.clone();
            handles.push(std::thread::spawn(move || {
                store.transact(1, |_, _| {
                    barrier.wait();
                    Ok(())
                })
            }));
        }
        let results = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| {
                    result
                        .as_ref()
                        .is_err_and(|error| error.code() == "error.project_catalog_stale_epoch")
                })
                .count(),
            1
        );
        assert_eq!(store.snapshot().unwrap().epoch(), 2);
    }

    #[test]
    fn shared_bridge_lock_and_v2_owner_contend_on_the_same_inode() {
        let (_directory, path) = projects_path();
        let registry = Arc::new(RwLock::new(
            crate::projects::ProjectRegistry::open(&path).unwrap(),
        ));
        let persister = StorePersister::spawn("catalog-lock-contention", registry, path.clone());
        let guard = acquire_store_lock_nofollow(&path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let result = persister.flush_blocking();
            sender.send(result).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());
        drop(guard);
        receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn migration_preflight_and_bridge_persister_contend_on_the_same_inode() {
        let (_directory, path) = projects_path();
        let (captured_sender, captured_receiver) = std::sync::mpsc::channel();
        let (release_sender, release_receiver) = std::sync::mpsc::channel();
        let capture_path = path.clone();
        let capture = std::thread::spawn(move || {
            capture_migration_preflight(&capture_path, || {
                captured_sender.send(()).unwrap();
                release_receiver.recv().unwrap();
                Ok(())
            })
        });
        captured_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let registry = Arc::new(RwLock::new(
            crate::projects::ProjectRegistry::open(&path).unwrap(),
        ));
        let persister = StorePersister::spawn("preflight-lock-contention", registry, path.clone());
        let (persisted_sender, persisted_receiver) = std::sync::mpsc::channel();
        let persist = std::thread::spawn(move || {
            persisted_sender.send(persister.flush_blocking()).unwrap();
        });
        assert!(
            persisted_receiver
                .recv_timeout(Duration::from_millis(100))
                .is_err()
        );

        release_sender.send(()).unwrap();
        capture.join().unwrap().unwrap();
        persisted_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap()
            .unwrap();
        persist.join().unwrap();
    }

    #[test]
    fn migration_apply_contends_on_code_owned_auxiliary_store_locks() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let auxiliary_path = path
            .parent()
            .unwrap()
            .join("code-source/effective-source-manifest.json");
        let auxiliary_guard = acquire_store_lock_nofollow(&auxiliary_path).unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            sender.send(transact_migration(&path, plan)).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(100)).is_err());

        drop(auxiliary_guard);
        receiver
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn migration_apply_contends_on_accepted_publication_and_publisher_source_locks() {
        for lane in ["accepted", "publisher"] {
            let (_directory, path, plan, _, _) = migration_fault_fixture();
            let root = path.parent().unwrap();
            let auxiliary_path = match lane {
                "accepted" => root.join("accepted-publications.json"),
                "publisher" => root.join("publisher-refs.json"),
                _ => unreachable!(),
            };
            let auxiliary_guard = acquire_store_lock_nofollow(&auxiliary_path).unwrap();
            let (sender, receiver) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                sender.send(transact_migration(&path, plan)).unwrap();
            });
            assert!(
                receiver.recv_timeout(Duration::from_millis(100)).is_err(),
                "{lane} auxiliary lock did not exclude migration apply"
            );
            drop(auxiliary_guard);
            receiver
                .recv_timeout(Duration::from_secs(3))
                .unwrap()
                .unwrap();
            worker.join().unwrap();
        }
    }

    #[test]
    fn migration_auxiliary_lock_order_is_lexically_deterministic() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let mut expected = plan.registry.auxiliary_store_paths();
        expected.sort();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&path, plan, recording.clone()).unwrap();
        let acquired = recording.mutation_lock_paths();
        assert_eq!(acquired.first(), Some(&path));
        assert_eq!(&acquired[1..], expected.as_slice());
    }

    #[test]
    fn migration_registry_rejects_unsafe_roots_and_duplicate_checkout_targets() {
        let (_directory, path) = projects_path();
        let root = path.parent().unwrap();
        assert!(
            MigrationParticipantRegistry::new(
                &path,
                root.join("code-source/../escape"),
                root.join("publisher-refs.json"),
                StoreLimits::default(),
            )
            .is_err()
        );

        let mut registry = MigrationParticipantRegistry::new(
            &path,
            root.join("code-source"),
            root.join("publisher-refs.json"),
            StoreLimits::default(),
        )
        .unwrap();
        registry
            .register_checkout_identity("first".into(), root.join("checkout"))
            .unwrap();
        registry
            .register_checkout_identity("second".into(), root.join("checkout"))
            .unwrap();
        assert!(registry.validate().is_err());
    }

    #[test]
    fn migration_registry_propagates_non_default_owner_limits_to_live_inventory() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let effective =
            encode_migration_effective_source_manifest_v1(&MigrationEffectiveSourceManifestV1 {
                version: 1,
                selections: Vec::new(),
            })
            .unwrap();
        let limits = StoreLimits {
            max_migration_survivor_bytes: effective.len() - 1,
            ..StoreLimits::default()
        };
        let (registry, mut draft, _) = basic_migration_draft_with_limits(&path, legacy, limits);
        let anchor = registry.code_source_paths.anchor();
        fs::create_dir_all(anchor.parent().unwrap()).unwrap();
        fs::write(&anchor, &effective).unwrap();
        draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == ParticipantRoleV1::EffectiveSourceManifest)
            .unwrap()
            .expected_old_sha256 = Some(Sha256Hex::digest(&effective));
        let inventory = enumerate_legacy_migration_inventory_for_scopes_locked(
            &registry.code_source_paths,
            &StoreLimits::default(),
            &published_catalog_scopes(&draft.catalog),
        )
        .unwrap();
        let code_source_inventory_sha256 =
            Sha256Hex::parse(inventory.canonical_sha256.clone()).unwrap();
        draft.inventory_sha256 = code_source_inventory_sha256.clone();
        draft.code_source_inventory_sha256 = code_source_inventory_sha256;
        draft.code_source_snapshot.legacy_inventory = inventory;
        let plan = validate_migration_plan(&path, registry, draft).unwrap();

        let error = transact_migration(&path, plan).unwrap_err();

        assert_eq!(
            error.code(),
            "error.project_catalog_migration_inventory_stale"
        );
        assert!(
            error
                .to_string()
                .contains("protected legacy inventory exceeds its aggregate byte limit"),
            "{error}"
        );
    }

    #[test]
    fn shared_checkout_roots_require_one_registry_binding_and_one_exact_id() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        let add_nested = |draft: &mut MigrationPlanDraftV1, checkout_id: &str| {
            let project_id = ProjectId::parse("nested-project").unwrap();
            draft.catalog.projects.insert(
                project_id.clone(),
                CorpusProject {
                    project_id: project_id.clone(),
                    scope: ProjectScope::LegacyLocal,
                    operator_aliases: BTreeSet::new(),
                    nominated_aliases: BTreeSet::new(),
                    display_name: "Nested project".into(),
                    created_at: "2026-07-23T00:00:00Z".into(),
                    registered_at_compat: None,
                    repo_history: None,
                    languages: BTreeSet::new(),
                },
            );
            let checkout_root = draft
                .attachments
                .attachments
                .values()
                .next()
                .unwrap()
                .checkout_dir
                .clone();
            let attachment_id =
                AttachmentId::parse("att_88888888888888888888888888888888").unwrap();
            draft.attachments.attachments.insert(
                attachment_id.clone(),
                CheckoutAttachment {
                    attachment_id,
                    project_id,
                    checkout_id: checkout_id.into(),
                    checkout_dir: checkout_root.clone(),
                    checkout_project_dir: format!("{checkout_root}/nested"),
                    project_root_relpath: "nested".into(),
                    kind: AttachmentKind::Base,
                    validated_scope: None,
                    computed_repo_hint: None,
                    branch_ref: None,
                    capabilities: AttachmentCapabilities {
                        local_code_source: true,
                        ..AttachmentCapabilities::default()
                    },
                    status: AttachmentStatus::Attached,
                    attached_at: "2026-07-23T00:00:00Z".into(),
                    detached_at: None,
                },
            );
        };

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_nested(&mut draft, "66666666666666666666666666666666");
        validate_migration_plan(&path, registry, draft).unwrap();

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_nested(&mut draft, "99999999999999999999999999999999");
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    #[test]
    fn catalog_and_auxiliary_lock_path_collisions_are_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(ProjectCatalogPaths::derive(&root.join("project-catalog-migration.lock")).is_err());
        assert!(
            MigrationParticipantRegistry::new(
                &root.join("effective-source-manifest.toml"),
                root.clone(),
                root.join("publisher-refs.json"),
                StoreLimits::default(),
            )
            .is_err()
        );
        assert!(
            MigrationParticipantRegistry::new(
                &root.join("projects.json"),
                root.join("project-catalog-stage/code-source"),
                root.join("publisher-refs.json"),
                StoreLimits::default(),
            )
            .is_err()
        );

        let mut registry = MigrationParticipantRegistry::new(
            &root.join("projects.json"),
            root.join("code-source"),
            root.join("publisher-refs.json"),
            StoreLimits::default(),
        )
        .unwrap();
        registry
            .register_checkout_identity(
                "nested-checkout".into(),
                root.join("project-catalog-backups/checkout"),
            )
            .unwrap();
        assert!(registry.validate().is_err());
    }

    #[test]
    fn invalid_migration_images_and_source_binding_have_no_side_effects() {
        let (directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n";

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.participants[0].post_image = Some(vec![b'x'; MAX_PROJECT_CATALOG_BYTES + 1]);
        assert!(validate_migration_plan(&path, registry, draft).is_err());
        assert_no_migration_outputs(&path);

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.immutable_assets[0].source = MigrationImmutableAssetSourceV1::InstallableBytes(vec![
                b'x';
                MAX_PROJECT_CATALOG_BYTES + 1
            ]);
        assert!(validate_migration_plan(&path, registry, draft).is_err());
        assert_no_migration_outputs(&path);

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.legacy_project_source =
            MigrationLegacyProjectSourceDraftV1::Present(b"different source".to_vec());
        assert!(validate_migration_plan(&path, registry, draft).is_err());
        assert_no_migration_outputs(&path);

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.immutable_assets[0].source = MigrationImmutableAssetSourceV1::InstallableBytes(
            b"different retained source".to_vec(),
        );
        assert!(validate_migration_plan(&path, registry, draft).is_err());
        assert_no_migration_outputs(&path);

        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.publisher_ref_source =
            MigrationPublisherSourceDraftV1::Present(b"different publisher source".to_vec());
        assert!(validate_migration_plan(&path, registry, draft).is_err());
        assert_no_migration_outputs(&path);

        drop(directory);
    }

    #[test]
    fn migration_marker_bytes_are_deterministic_for_a_validated_plan() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        let (registry, draft, _) = basic_migration_draft(&path, legacy);
        let first = validate_migration_plan(&path, registry.clone(), draft.clone()).unwrap();
        let second = validate_migration_plan(&path, registry, draft).unwrap();
        assert_eq!(
            first.post_images.get(&ParticipantRoleV1::MigrationMarker),
            second.post_images.get(&ParticipantRoleV1::MigrationMarker)
        );
    }

    #[test]
    fn present_legacy_project_source_survives_crash_retry_and_verification() {
        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let retry = plan.clone();
        let registry = plan.registry.clone();
        let expected_identity = plan.artifact_identity();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));

        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        assert_eq!(transact_migration(&path, retry).unwrap().epoch, 1);

        let store =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), registry.clone())
                .unwrap();
        assert_eq!(
            store.migration_artifact_identity().unwrap(),
            expected_identity
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let marker: ProjectCatalogMigrationMarkerV1 = decode_bounded_json(
            &fs::read(paths.migration_marker).unwrap(),
            MAX_MARKER_BYTES,
            "migration marker",
        )
        .unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        let expected_hash = sha256(&legacy_bytes);
        assert_eq!(
            marker.legacy_project_source,
            MigrationLegacyProjectSourceEvidenceV1::Present {
                sha256: expected_hash.clone(),
            }
        );
        assert_eq!(
            journal.legacy_project_source,
            Some(MigrationLegacyProjectSourceEvidenceV1::Present {
                sha256: expected_hash.clone(),
            })
        );
        assert_eq!(
            journal
                .participants
                .iter()
                .find(|participant| participant.role == ParticipantRoleV1::Catalog)
                .and_then(|participant| participant.old.sha256()),
            Some(&expected_hash)
        );
        let source_backup = marker
            .immutable_assets
            .iter()
            .find(|asset| asset.role == ImmutableAssetRoleV1::LegacyProjectStoreBackup)
            .unwrap();
        assert_eq!(source_backup.sha256, expected_hash);
        assert_eq!(
            fs::read(registry.immutable_target(&source_backup.role, &source_backup.validated_name))
                .unwrap(),
            legacy_bytes
        );
    }

    #[test]
    fn absent_legacy_project_source_survives_crash_retry_and_verification_without_backup() {
        let (_directory, path, plan, _) = missing_source_migration_fault_fixture();
        let retry = plan.clone();
        let registry = plan.registry.clone();
        let expected_identity = plan.artifact_identity();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));

        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        assert_eq!(transact_migration(&path, retry).unwrap().epoch, 1);

        let store =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), registry).unwrap();
        assert_eq!(
            store.migration_artifact_identity().unwrap(),
            expected_identity
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let marker: ProjectCatalogMigrationMarkerV1 = decode_bounded_json(
            &fs::read(paths.migration_marker).unwrap(),
            MAX_MARKER_BYTES,
            "migration marker",
        )
        .unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        let expected = MigrationLegacyProjectSourceEvidenceV1::missing();
        assert_eq!(marker.legacy_project_source, expected);
        assert_eq!(journal.legacy_project_source, Some(expected));
        assert!(
            journal
                .participants
                .iter()
                .find(|participant| participant.role == ParticipantRoleV1::Catalog)
                .is_some_and(|participant| matches!(participant.old, ExpectedImageV1::Absent {}))
        );
        assert!(
            marker
                .immutable_assets
                .iter()
                .all(|asset| asset.role != ImmutableAssetRoleV1::LegacyProjectStoreBackup)
        );
        assert!(
            journal
                .immutable_assets
                .iter()
                .all(|asset| asset.role != ImmutableAssetRoleV1::LegacyProjectStoreBackup)
        );
        assert!(
            !expected_identity
                .immutable_assets
                .iter()
                .any(|asset| asset.role == ImmutableAssetRoleV1::LegacyProjectStoreBackup)
        );
    }

    #[test]
    fn publisher_evidence_requires_exactly_one_matching_disposition_per_pin() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";

        let (registry, mut draft, _, _) = publisher_seed_migration_draft(&path, legacy);
        draft.publisher_dispositions.clear();
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_invalid_publisher_evidence"
        );

        let (registry, mut draft, _, _) = publisher_seed_migration_draft(&path, legacy);
        draft
            .publisher_dispositions
            .push(draft.publisher_dispositions[0].clone());
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_invalid_publisher_evidence"
        );

        let (registry, mut draft, _, _) = publisher_seed_migration_draft(&path, legacy);
        if let PublisherDispositionEvidenceV1::SeedG1 { observation_id, .. } =
            &mut draft.publisher_dispositions[0]
        {
            *observation_id = "different-observation".into();
        }
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_invalid_publisher_evidence"
        );
    }

    #[test]
    fn accepted_pointer_and_generation_require_exact_seed_binding() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        let (registry, draft, _, _) = publisher_seed_migration_draft(&path, legacy);
        validate_migration_plan(&path, registry, draft).unwrap();

        let (registry, mut draft, pointer_role, _) = publisher_seed_migration_draft(&path, legacy);
        draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == pointer_role)
            .unwrap()
            .post_image = Some(b"{}".to_vec());
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");

        let (registry, mut draft, _, generation_role) =
            publisher_seed_migration_draft(&path, legacy);
        let source = &mut draft
            .immutable_assets
            .iter_mut()
            .find(|asset| asset.role == generation_role)
            .unwrap()
            .source;
        *source = MigrationImmutableAssetSourceV1::InstallableBytes(b"{}".to_vec());
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    #[test]
    fn activation_requires_its_exact_scope_bearing_stored_generation() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        let (registry, draft, _, _) = activation_migration_draft(&path, legacy);
        validate_migration_plan(&path, registry, draft).unwrap();

        let (registry, mut draft, _, stored_role) = activation_migration_draft(&path, legacy);
        draft
            .participants
            .retain(|participant| participant.role != stored_role);
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");

        let (registry, mut draft, activation_role, _) = activation_migration_draft(&path, legacy);
        let participant = draft
            .participants
            .iter_mut()
            .find(|participant| participant.role == activation_role)
            .unwrap();
        let mut activation =
            decode_activation_v2_for_migration(participant.post_image.as_deref().unwrap()).unwrap();
        activation.document_count = 1;
        participant.post_image =
            Some(bbox_code_source_store::encode_activation_v2_for_migration(&activation).unwrap());
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    #[test]
    fn pinned_collected_manifest_is_verified_without_an_installable_stage() {
        let (_directory, path, plan, manifest_target, manifest_bytes) =
            extended_migration_fault_fixture();
        let pinned = plan
            .journal
            .immutable_assets
            .iter()
            .find(|asset| {
                matches!(
                    &asset.role,
                    ImmutableAssetRoleV1::CollectedGenerationManifest { .. }
                )
            })
            .unwrap();
        assert_eq!(pinned.mode, ImmutableAssetModeV1::PinnedExisting);
        assert!(pinned.stage_name.is_none());
        assert!(!plan.immutable_asset_bytes.contains_key(&pinned.role));

        transact_migration(&path, plan).unwrap();
        assert_eq!(fs::read(manifest_target).unwrap(), manifest_bytes);
    }

    #[test]
    fn collision_lifecycle_can_be_installed_from_an_absent_preimage() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            "first-collision-project",
            "first-collision-repo",
            "first-collision-producer",
            "first-collision-observation",
            false,
        );
        let project_id = ProjectId::parse("first-collision-project").unwrap();
        let lifecycle_path = registry
            .code_source_paths
            .collision_retirement_pending(&project_id);
        let participant = draft
            .participants
            .iter()
            .find(|participant| {
                participant.role
                    == (ParticipantRoleV1::CollisionRetirement {
                        project_id: project_id.clone(),
                    })
            })
            .unwrap();
        assert!(participant.expected_old_sha256.is_none());
        assert!(!lifecycle_path.exists());
        let plan = validate_migration_plan(&path, registry, draft).unwrap();

        transact_migration(&path, plan).unwrap();

        let lifecycle =
            decode_collision_retirement_pending_for_migration(&fs::read(lifecycle_path).unwrap())
                .unwrap();
        assert!(
            lifecycle
                .entries
                .values()
                .all(|entry| entry.state == CollisionRetirementLifecycleStateV1::Pending)
        );
    }

    #[test]
    fn retained_only_collision_journal_recovers_without_activation_authority() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        let project_id = ProjectId::parse("retained-only-collision").unwrap();
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            project_id.as_str(),
            "retained-only-repo",
            "retained-only-producer",
            "retained-only-observation",
            false,
        );
        let retained_generation_id =
            make_collision_retained_only(&registry, &mut draft, &project_id);
        let plan = validate_migration_plan(&path, registry.clone(), draft).unwrap();
        assert!(!plan.journal.participants.iter().any(|participant| {
            participant.role
                == (ParticipantRoleV1::Activation {
                    project_id: project_id.clone(),
                })
        }));
        plan.journal.validate().unwrap();

        let failing = Arc::new(TracingIo::failing_points([FaultPoint::CompletePlanVerify]));
        let error = transact_migration_with_io(&path, plan, failing.clone()).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        let trace = failing.trace();
        assert!(trace.contains(&FaultPoint::PreparedJournalWrite));
        assert!(trace.contains(&FaultPoint::ParticipantInstall));
        assert!(trace.contains(&FaultPoint::CompletePlanVerify));
        assert!(!trace.contains(&FaultPoint::CommittedJournalWrite));
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let interrupted: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(interrupted.state, TransactionStateV1::Prepared);
        assert_eq!(interrupted.outcome, None);
        assert!(
            registry
                .code_source_paths
                .collision_retirement_pending(&project_id)
                .exists()
        );
        recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo)).unwrap();

        let lifecycle_path = registry
            .code_source_paths
            .collision_retirement_pending(&project_id);
        let lifecycle =
            decode_collision_retirement_pending_for_migration(&fs::read(lifecycle_path).unwrap())
                .unwrap();
        assert_eq!(
            lifecycle.entries.keys().cloned().collect::<BTreeSet<_>>(),
            BTreeSet::from([retained_generation_id.to_string()])
        );
        assert!(
            lifecycle
                .entries
                .values()
                .all(|entry| entry.exact_selector().is_none())
        );
        assert!(!registry.code_source_paths.activation(&project_id).exists());
        ProjectCatalogStore::open_existing_after_migration(path, registry).unwrap();
    }

    #[test]
    fn retained_only_preinstall_forward_recovery_uses_staged_scope_evidence() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        let project_id = ProjectId::parse("retained-only-collision").unwrap();
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            project_id.as_str(),
            "retained-only-repo",
            "retained-only-producer",
            "retained-only-observation",
            false,
        );
        make_collision_retained_only(&registry, &mut draft, &project_id);
        let plan = validate_migration_plan(&path, registry.clone(), draft).unwrap();

        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        let lifecycle_path = registry
            .code_source_paths
            .collision_retirement_pending(&project_id);
        assert!(!lifecycle_path.exists());

        recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo)).unwrap();
        let lifecycle =
            decode_collision_retirement_pending_for_migration(&fs::read(lifecycle_path).unwrap())
                .unwrap();
        assert!(
            lifecycle
                .entries
                .values()
                .all(|entry| entry.exact_selector().is_none())
        );
        assert!(!registry.code_source_paths.activation(&project_id).exists());
    }

    #[test]
    fn retained_only_forward_recovery_refuses_untrusted_staged_lifecycle_scope_evidence() {
        for mutation in ["missing", "mismatched_scope", "corrupt"] {
            let (_directory, path) = projects_path();
            let legacy = b"{\"version\":1,\"projects\":[]}\n";
            fs::write(&path, legacy).unwrap();
            let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
            let project_id = ProjectId::parse("retained-only-collision").unwrap();
            add_named_collision_retirement_to_draft(
                &registry,
                &mut draft,
                project_id.as_str(),
                "retained-only-repo",
                "retained-only-producer",
                "retained-only-observation",
                false,
            );
            make_collision_retained_only(&registry, &mut draft, &project_id);
            let plan = validate_migration_plan(&path, registry.clone(), draft).unwrap();

            let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
            let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
            assert_eq!(error.code(), "error.project_catalog_injected_fault");
            let paths = ProjectCatalogPaths::derive(&path).unwrap();
            let prepared: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(&paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(prepared.state, TransactionStateV1::Prepared);
            let collision = prepared
                .participants
                .iter()
                .find(|participant| {
                    participant.role
                        == (ParticipantRoleV1::CollisionRetirement {
                            project_id: project_id.clone(),
                        })
                })
                .unwrap();
            let ExpectedImageV1::Present { artifact_name, .. } = &collision.new else {
                unreachable!();
            };
            let staged_lifecycle = paths.stage_dir.join(artifact_name.as_str());
            match mutation {
                "missing" => fs::remove_file(&staged_lifecycle).unwrap(),
                "mismatched_scope" => {
                    let mut lifecycle = decode_collision_retirement_pending_for_migration(
                        &fs::read(&staged_lifecycle).unwrap(),
                    )
                    .unwrap();
                    lifecycle.entries.values_mut().next().unwrap().former_scope =
                        PublishedScope::try_new("different-repo", "different-root").unwrap();
                    fs::write(
                        &staged_lifecycle,
                        bbox_code_source_store::encode_collision_retirement_pending_for_migration(
                            &lifecycle,
                        )
                        .unwrap(),
                    )
                    .unwrap();
                }
                "corrupt" => fs::write(&staged_lifecycle, b"{not-json").unwrap(),
                _ => unreachable!(),
            }

            recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo))
                .unwrap();
            let recovered: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(&paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(
                recovered.outcome,
                Some(TransactionOutcomeV1::RolledBack),
                "{mutation} staged lifecycle evidence must not permit forward recovery"
            );
            assert!(
                !registry
                    .code_source_paths
                    .collision_retirement_pending(&project_id)
                    .exists()
            );
        }
    }

    #[test]
    fn post_install_forward_recovery_checks_live_inventory_before_commit() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        let project_id = ProjectId::parse("retained-only-collision").unwrap();
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            project_id.as_str(),
            "retained-only-repo",
            "retained-only-producer",
            "retained-only-observation",
            false,
        );
        make_collision_retained_only(&registry, &mut draft, &project_id);
        let plan = validate_migration_plan(&path, registry.clone(), draft).unwrap();
        let retry = plan.clone();

        let failing = Arc::new(TracingIo::failing_points([FaultPoint::CompletePlanVerify]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        let lifecycle_path = registry
            .code_source_paths
            .collision_retirement_pending(&project_id);
        assert!(lifecycle_path.exists());
        let installed_lifecycle =
            decode_collision_retirement_pending_for_migration(&fs::read(&lifecycle_path).unwrap())
                .unwrap();
        let unprotected_scope = installed_lifecycle
            .entries
            .values()
            .next()
            .unwrap()
            .former_scope
            .clone();
        let unprotected_generation = write_unprotected_legacy_generation_in_scope(
            &registry.code_source_paths,
            unprotected_scope.clone(),
        );

        recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo)).unwrap();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let rolled_back: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(rolled_back.outcome, Some(TransactionOutcomeV1::RolledBack));
        assert!(!lifecycle_path.exists());

        let unprotected_metadata = registry
            .code_source_paths
            .generation_metadata(&unprotected_scope, &unprotected_generation)
            .unwrap();
        fs::remove_dir_all(unprotected_metadata.parent().unwrap()).unwrap();
        transact_migration(&path, retry).unwrap();
        assert!(lifecycle_path.exists());
    }

    #[test]
    fn collision_activation_participant_exactly_mirrors_selector_authority() {
        let (_directory, retained_path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&retained_path, legacy).unwrap();
        let (retained_registry, mut retained_draft, _) =
            basic_migration_draft(&retained_path, legacy);
        let retained_project = ProjectId::parse("retained-only-collision").unwrap();
        add_named_collision_retirement_to_draft(
            &retained_registry,
            &mut retained_draft,
            retained_project.as_str(),
            "retained-only-repo",
            "retained-only-producer",
            "retained-only-observation",
            false,
        );
        make_collision_retained_only(&retained_registry, &mut retained_draft, &retained_project);
        let mut retained_plan =
            validate_migration_plan(&retained_path, retained_registry, retained_draft).unwrap();
        retained_plan.post_images.insert(
            ParticipantRoleV1::Activation {
                project_id: retained_project,
            },
            None,
        );
        assert!(revalidate_plan_cross_roles(&retained_plan).is_err());

        let (_directory, exact_path) = projects_path();
        fs::write(&exact_path, legacy).unwrap();
        let (exact_registry, mut exact_draft, _) = basic_migration_draft(&exact_path, legacy);
        let exact_project = ProjectId::parse("exact-collision").unwrap();
        add_named_collision_retirement_to_draft(
            &exact_registry,
            &mut exact_draft,
            exact_project.as_str(),
            "exact-repo",
            "exact-producer",
            "exact-observation",
            false,
        );
        let mut exact_plan =
            validate_migration_plan(&exact_path, exact_registry, exact_draft).unwrap();
        let collision_role = ParticipantRoleV1::CollisionRetirement {
            project_id: exact_project.clone(),
        };
        let mut multiple_exact_plan = exact_plan.clone();
        let retirement_bytes = multiple_exact_plan
            .post_images
            .get_mut(&collision_role)
            .unwrap()
            .as_mut()
            .unwrap();
        let mut retirement =
            decode_collision_retirement_pending_for_migration(retirement_bytes).unwrap();
        let retained_generation_id = retirement
            .entries
            .iter()
            .find_map(|(generation_id, entry)| {
                entry
                    .exact_selector()
                    .is_none()
                    .then_some(generation_id.clone())
            })
            .unwrap();
        retirement
            .entries
            .get_mut(&retained_generation_id)
            .unwrap()
            .selector_evidence = CollisionRetirementSelectorEvidenceV1::ExactMaterialized(
            historical_selector(exact_project.as_str(), &retained_generation_id),
        );
        *retirement_bytes =
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&retirement)
                .unwrap();
        assert!(revalidate_plan_cross_roles(&multiple_exact_plan).is_err());

        exact_plan
            .post_images
            .remove(&ParticipantRoleV1::Activation {
                project_id: exact_project.clone(),
            });
        assert!(revalidate_plan_cross_roles(&exact_plan).is_err());
    }

    #[test]
    fn existing_collision_lifecycle_refuses_membership_and_state_rewrites() {
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            let generation_id = lifecycle.entries.keys().next().unwrap().clone();
            lifecycle.entries.remove(&generation_id);
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            let mut entry = lifecycle.entries.values().next().unwrap().clone();
            entry.selector_evidence =
                bbox_code_source_store::CollisionRetirementSelectorEvidenceV1::NoDurableSelector;
            lifecycle.entries.insert("9".repeat(64), entry);
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle.entries.values_mut().next().unwrap().state =
                CollisionRetirementLifecycleStateV1::Queued;
        });
    }

    #[test]
    fn existing_collision_lifecycle_refuses_every_immutable_evidence_splice() {
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle.entries.values_mut().next().unwrap().former_scope =
                PublishedScope::try_new("different-repo", ".").unwrap();
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            let entry = lifecycle
                .entries
                .values_mut()
                .find(|entry| entry.exact_selector().is_some())
                .unwrap();
            let CollisionRetirementSelectorEvidenceV1::ExactMaterialized(selector) =
                &mut entry.selector_evidence
            else {
                unreachable!();
            };
            selector.truncate(selector.len() - 16);
            selector.push_str("ffffffffffffffff");
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle.entries.values_mut().next().unwrap().snapshot_id =
                format!("collected-{}", "9".repeat(32));
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle
                .entries
                .values_mut()
                .next()
                .unwrap()
                .manifest_sha256 = "9".repeat(64);
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle
                .entries
                .values_mut()
                .next()
                .unwrap()
                .inventory_hash = "9".repeat(64);
        });
        assert_existing_collision_lifecycle_mutation_refused(|lifecycle| {
            lifecycle.entries.values_mut().next().unwrap().plan_hash = "9".repeat(64);
        });
    }

    #[test]
    fn resolved_collision_owner_binding_refuses_retained_reassignment_and_omission() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            "resolved-collision-project",
            "resolved-collision-repo",
            "resolved-collision-producer",
            "resolved-collision-observation",
            false,
        );
        let participant = draft
            .participants
            .iter_mut()
            .find(|participant| {
                matches!(
                    participant.role,
                    ParticipantRoleV1::CollisionRetirement { .. }
                )
            })
            .unwrap();
        let mut lifecycle = decode_collision_retirement_pending_for_migration(
            participant.post_image.as_deref().unwrap(),
        )
        .unwrap();
        let retained_generation_id = lifecycle
            .entries
            .iter()
            .find_map(|(generation_id, entry)| {
                entry
                    .exact_selector()
                    .is_none()
                    .then_some(generation_id.clone())
            })
            .unwrap();
        lifecycle.entries.remove(&retained_generation_id);
        participant.post_image = Some(
            bbox_code_source_store::encode_collision_retirement_pending_for_migration(&lifecycle)
                .unwrap(),
        );
        let retained = draft
            .code_source_snapshot
            .generations
            .iter_mut()
            .find(|generation| generation.generation_id.as_str() == retained_generation_id)
            .unwrap();
        retained.project_id = ProjectId::parse("checkout-project").unwrap();
        retained.disposition = MigrationCodeSourceDispositionV1::SurvivingRetained;

        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
        assert!(
            error
                .to_string()
                .contains("different canonical generation owners")
        );

        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy);
        add_named_collision_retirement_to_draft(
            &registry,
            &mut draft,
            "resolved-collision-project",
            "resolved-collision-repo",
            "resolved-collision-producer",
            "resolved-collision-observation",
            false,
        );
        add_retained_generation_to_draft(&registry, &mut draft);
        let winner_project_id = ProjectId::parse("retained-project").unwrap();
        let collision_project_id = ProjectId::parse("resolved-collision-project").unwrap();
        let winner = draft
            .code_source_snapshot
            .generations
            .iter_mut()
            .find(|generation| generation.project_id == winner_project_id)
            .unwrap();
        let winner_generation_id = winner.generation_id.clone();
        winner.project_id = collision_project_id.clone();
        let winner_participant = draft
            .participants
            .iter_mut()
            .find(|participant| {
                matches!(
                    &participant.role,
                    ParticipantRoleV1::StoredGenerationMetadata { generation_id, .. }
                        if generation_id == &winner_generation_id
                )
            })
            .unwrap();
        let ParticipantRoleV1::StoredGenerationMetadata { project_id, .. } =
            &mut winner_participant.role
        else {
            unreachable!();
        };
        *project_id = collision_project_id;

        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
        assert!(
            error
                .to_string()
                .contains("different canonical generation owners")
        );
    }

    #[test]
    fn missing_or_corrupt_pinned_manifest_refuses_apply_but_allows_rollback() {
        let (_directory, path, plan, manifest_target, _) = extended_migration_fault_fixture();
        fs::remove_file(&manifest_target).unwrap();
        let error = transact_migration(&path, plan).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_migration_inventory_stale"
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());

        let (_directory, path, plan, manifest_target, _) = extended_migration_fault_fixture();
        fs::write(&manifest_target, b"corrupt manifest").unwrap();
        let error = transact_migration(&path, plan).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_artifact_collision");
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());

        let (_directory, path, plan, manifest_target, _) = extended_migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        fs::remove_file(&manifest_target).unwrap();
        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(ProjectCatalogPaths::derive(&path).unwrap().journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));

        let (_directory, path, plan, manifest_target, _) = extended_migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        fs::write(&manifest_target, b"corrupt manifest").unwrap();
        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(ProjectCatalogPaths::derive(&path).unwrap().journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));
    }

    #[test]
    fn migration_role_caps_are_specific_to_each_participant_class() {
        let project_id = ProjectId::parse("cap-project").unwrap();
        let scope = PublishedScope::try_new("cap-repo", ".").unwrap();
        assert_eq!(
            ParticipantRoleV1::AcceptedPublicationPointer {
                project_id: project_id.clone(),
            }
            .max_bytes(),
            MAX_ACCEPTED_PUBLICATION_POINTER_BYTES
        );
        assert_eq!(
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id,
                published_scope: scope,
                generation_id: Sha256Hex::digest(b"cap-generation"),
            }
            .max_bytes(),
            MAX_CODE_SOURCE_GENERATION_METADATA_BYTES
        );
        assert!(ParticipantRoleV1::MigrationMarker.max_bytes() > MAX_PROJECT_CATALOG_BYTES);
        assert!(ParticipantRoleV1::EffectiveSourceManifest.max_bytes() > MAX_PROJECT_CATALOG_BYTES);
    }

    #[test]
    fn migration_evidence_cardinality_limits_fail_closed() {
        let (_directory, path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n";
        let (registry, mut draft, _) = basic_migration_draft(&path, legacy_bytes);
        draft.participants = std::iter::repeat_with(|| {
            MigrationParticipantDraftV1::new(ParticipantRoleV1::EffectiveSourceManifest, None, None)
        })
        .take(MAX_MIGRATION_PARTICIPANTS)
        .collect();
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
        assert_no_migration_outputs(&path);

        let (registry, mut draft, _, _) = publisher_seed_migration_draft(&path, legacy_bytes);
        draft.publisher_pins =
            vec![draft.publisher_pins[0].clone(); MAX_MIGRATION_PUBLISHER_PINS + 1];
        let error = validate_migration_plan(&path, registry, draft).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_migration_plan");
    }

    fn maximum_scope_structural_evidence() -> (TransactionParticipantV1, ImmutableAssetV1) {
        let max_root = [
            std::iter::repeat_n("p".repeat(255), 15).collect::<Vec<_>>(),
            vec!["q".repeat(254), "r".into()],
        ]
        .concat()
        .join("/");
        assert_eq!(max_root.len(), 4096);
        let scope = PublishedScope::try_new("s".repeat(256), max_root).unwrap();
        let transaction_id = ProjectCatalogTransactionId::mint();
        let generation_id = Sha256Hex::digest(b"maximum-scope-generation");
        let participant = build_transaction_participant(
            &transaction_id,
            ParticipantRoleV1::StoredGenerationMetadata {
                project_id: ProjectId::parse("x".repeat(96)).unwrap(),
                published_scope: scope.clone(),
                generation_id: generation_id.clone(),
            },
            Some(Sha256Hex::digest(b"old metadata")),
            &Some(vec![b'x']),
        )
        .unwrap();
        let asset_role = ImmutableAssetRoleV1::CollectedGenerationManifest {
            published_scope: scope,
            generation_id,
        };
        let asset_hash = Sha256Hex::digest(b"manifest bytes");
        let asset = ImmutableAssetV1 {
            validated_name: immutable_target_name(&transaction_id, &asset_role, &asset_hash)
                .unwrap(),
            stage_name: None,
            role: asset_role,
            mode: ImmutableAssetModeV1::PinnedExisting,
            sha256: asset_hash,
        };
        (participant, asset)
    }

    #[test]
    fn durable_structural_evidence_budget_accepts_the_exact_encoded_boundary() {
        assert_eq!(MAX_MARKER_BYTES, MAX_JOURNAL_BYTES);
        assert_eq!(
            MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES
                + MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES
                + MAX_MIGRATION_DURABLE_ENVELOPE_BYTES,
            MAX_JOURNAL_BYTES
        );
        let (participant, asset) = maximum_scope_structural_evidence();
        let combined_charge = nested_pretty_json_row_charge(&participant)
            .unwrap()
            .checked_add(nested_pretty_json_row_charge(&asset).unwrap())
            .unwrap();
        assert!(combined_charge <= MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES);

        let mut exact = MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES - combined_charge;
        add_durable_structural_evidence_size(&mut exact, &participant, "test journal").unwrap();
        add_durable_structural_evidence_size(&mut exact, &asset, "test journal").unwrap();
        assert_eq!(exact, MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES);
    }

    #[test]
    fn durable_structural_evidence_budget_refuses_the_first_excess_byte() {
        let (participant, asset) = maximum_scope_structural_evidence();
        let combined_charge = nested_pretty_json_row_charge(&participant)
            .unwrap()
            .checked_add(nested_pretty_json_row_charge(&asset).unwrap())
            .unwrap();
        assert!(combined_charge <= MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES);
        let mut excess = MAX_MIGRATION_DURABLE_STRUCTURAL_EVIDENCE_BYTES - combined_charge + 1;
        add_durable_structural_evidence_size(&mut excess, &participant, "test journal").unwrap();
        let error =
            add_durable_structural_evidence_size(&mut excess, &asset, "test journal").unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_durable_evidence_exhausted"
        );
        assert!(
            error
                .to_string()
                .contains("aggregate durable-evidence budget is exhausted")
        );
    }

    #[test]
    fn publisher_evidence_aggregate_budget_rejects_the_first_excess_byte() {
        let evidence = PublisherDispositionEvidenceV1::NoPublishedContentAcknowledged {
            observation_id: "publisher-observation".into(),
            project_id: ProjectId::parse("published-project").unwrap(),
            expected_scope: PublishedScope::try_new("published-repo", ".").unwrap(),
            full_ref: FullPublisherRef::parse("refs/heads/main").unwrap(),
            bounded_reason: "bounded reason".into(),
        };
        let charged = nested_pretty_json_row_charge(&evidence).unwrap();
        let mut exact = MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES - charged;
        add_publisher_evidence_size(&mut exact, &evidence, "test").unwrap();
        assert_eq!(exact, MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES);

        let mut excess = MAX_MIGRATION_PUBLISHER_EVIDENCE_BYTES - charged + 1;
        let error = add_publisher_evidence_size(&mut excess, &evidence, "test").unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_invalid_publisher_evidence"
        );
    }

    #[test]
    fn migration_replaces_only_an_empty_checkout_identity_marker() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let marker = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        fs::write(&marker, b"").unwrap();

        transact_migration(&path, plan).unwrap();
        assert_eq!(
            fs::read_to_string(marker).unwrap(),
            "66666666666666666666666666666666"
        );
    }

    #[test]
    fn checkout_local_state_is_never_created_before_the_prepared_journal() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let root = path.parent().unwrap();
        let local = root.join("checkout/.bbox/local");
        let failing = Arc::new(TracingIo::failing_points([
            FaultPoint::PreparedJournalWrite,
        ]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.journal.exists());
        assert!(!local.exists());

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let root = path.parent().unwrap();
        let local = root.join("checkout/.bbox/local");
        let failing = Arc::new(TracingIo::failing_points([
            FaultPoint::MonotonicCheckoutIdentityAction,
        ]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(paths.journal.exists());
        assert!(local.exists());
        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        // Installed markers carry the runtime producer's bare shape, the
        // same bytes ensure_checkout_id writes.
        assert_eq!(
            fs::read_to_string(local.join("checkout-id")).unwrap(),
            "66666666666666666666666666666666"
        );
    }

    #[test]
    fn migration_refuses_a_different_checkout_identity_without_overwrite() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let marker = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let existing = b"77777777777777777777777777777777\n";
        fs::write(&marker, existing).unwrap();

        assert!(transact_migration(&path, plan).is_err());
        assert_eq!(fs::read(marker).unwrap(), existing);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());
        assert!(!paths.attachments.exists());
    }

    #[cfg(unix)]
    #[test]
    fn migration_refuses_a_symlinked_checkout_identity_without_overwrite() {
        use std::os::unix::fs::symlink;

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let marker = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::create_dir_all(marker.parent().unwrap()).unwrap();
        let outside = path.parent().unwrap().join("outside-checkout-id");
        let existing = b"77777777777777777777777777777777\n";
        fs::write(&outside, existing).unwrap();
        symlink(&outside, &marker).unwrap();

        assert!(transact_migration(&path, plan).is_err());
        assert_eq!(fs::read(outside).unwrap(), existing);
        assert!(
            fs::symlink_metadata(marker)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());
        assert!(!paths.attachments.exists());
    }

    #[cfg(unix)]
    #[test]
    fn strict_open_rejects_a_fifo_without_blocking() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        fs::remove_file(&paths.attachments).unwrap();
        let fifo = CString::new(paths.attachments.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

        let started = std::time::Instant::now();
        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_non_regular_file");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[cfg(unix)]
    #[test]
    fn strict_open_refuses_symlinked_participant() {
        use std::os::unix::fs::symlink;

        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let target = paths
            .attachments
            .parent()
            .unwrap()
            .join("attachment-target.json");
        fs::rename(&paths.attachments, &target).unwrap();
        symlink(&target, &paths.attachments).unwrap();

        assert!(ProjectCatalogStore::open_existing(path).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn initialization_refuses_symlinked_transaction_directory() {
        use std::os::unix::fs::symlink;

        let (_directory, path) = projects_path();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let redirected = path.parent().unwrap().join("redirected-stage");
        fs::create_dir(&redirected).unwrap();
        symlink(&redirected, &paths.stage_dir).unwrap();

        let error = ProjectCatalogStore::initialize_empty(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_io");
        assert!(fs::read_dir(&redirected).unwrap().next().is_none());
    }

    #[test]
    fn strict_journal_codec_rejects_duplicate_and_unknown_fields() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let text = fs::read_to_string(paths.journal).unwrap();

        let duplicate = format!("{{\"version\":1,{}", &text[1..]);
        assert!(
            decode_bounded_json::<ProjectCatalogTransactionJournalV1>(
                duplicate.as_bytes(),
                MAX_JOURNAL_BYTES,
                "transaction journal"
            )
            .is_err()
        );
        let unknown = text.replacen('{', "{\"unknown\":true,", 1);
        assert!(
            decode_bounded_json::<ProjectCatalogTransactionJournalV1>(
                unknown.as_bytes(),
                MAX_JOURNAL_BYTES,
                "transaction journal"
            )
            .is_err()
        );
    }

    #[test]
    fn migration_v1_codecs_require_exact_reviewed_artifact_identity() {
        let (_directory, _path, plan, _, _) = migration_fault_fixture();
        let marker_bytes = plan
            .post_images
            .get(&ParticipantRoleV1::MigrationMarker)
            .and_then(Option::as_deref)
            .unwrap();
        let mut marker: serde_json::Value = serde_json::from_slice(marker_bytes).unwrap();
        marker
            .as_object_mut()
            .unwrap()
            .remove("report_artifact_sha256");
        assert!(
            decode_bounded_json::<ProjectCatalogMigrationMarkerV1>(
                &serde_json::to_vec(&marker).unwrap(),
                MAX_MARKER_BYTES,
                "migration marker",
            )
            .is_err()
        );

        let mut journal = serde_json::to_value(&plan.journal).unwrap();
        let journal = journal.as_object_mut().unwrap();
        journal.remove("report_artifact_sha256");
        journal.remove("resolution_artifact_sha256");
        let decoded: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &serde_json::to_vec(journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(
            decoded.validate().unwrap_err().code(),
            "error.project_catalog_invalid_journal"
        );
    }

    #[test]
    fn journal_validation_enforces_epoch_images_and_unique_checkout_actions() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let mut regular: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        regular.old_epoch = 1;
        regular.new_epoch = 2;
        assert!(regular.validate().is_err());

        let (_migration_directory, _, plan, _, _) = migration_fault_fixture();
        let migration = plan.journal;
        migration.validate().unwrap();

        let mut missing_quarantine_authority = migration.clone();
        missing_quarantine_authority.resolved_quarantine_bindings = None;
        let error = missing_quarantine_authority.validate().unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_journal");
        assert!(error.to_string().contains("canonical quarantine bindings"));

        let mut wrong_epoch = migration.clone();
        wrong_epoch.old_epoch = 1;
        wrong_epoch.new_epoch = 2;
        assert!(wrong_epoch.validate().is_err());

        let mut missing_catalog_backup = migration.clone();
        missing_catalog_backup
            .participants
            .iter_mut()
            .find(|participant| participant.role == ParticipantRoleV1::Catalog)
            .unwrap()
            .old = ExpectedImageV1::Absent {};
        assert!(missing_catalog_backup.validate().is_err());

        let mut duplicate_action = migration;
        duplicate_action
            .monotonic_checkout_identity_actions
            .push(duplicate_action.monotonic_checkout_identity_actions[0].clone());
        assert!(duplicate_action.validate().is_err());
    }

    #[test]
    fn empty_initialization_fault_matrix_reopens_to_one_coherent_state() {
        let (_trace_directory, trace_path) = projects_path();
        let recording = Arc::new(TracingIo::recording());
        let store =
            ProjectCatalogStore::initialize_empty_with_io(trace_path, recording.clone()).unwrap();
        let initialized = state_fingerprint(&store.snapshot().unwrap());
        drop(store);
        let trace = recording.trace();
        assert!(trace.contains(&FaultPoint::PreparedJournalWrite));
        assert!(trace.contains(&FaultPoint::ParticipantInstall));
        assert!(trace.contains(&FaultPoint::CommittedJournalWrite));

        for index in 0..trace.len() {
            let (_directory, path) = projects_path();
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = ProjectCatalogStore::initialize_empty_with_io(path.clone(), failing);
            assert_known_state_or_absent(&path, std::slice::from_ref(&initialized));
            assert_retained_journal_artifacts(&path);
        }
    }

    #[test]
    fn nonempty_pair_fault_matrix_never_reopens_a_mixed_epoch() {
        let (_trace_directory, trace_path) = projects_path();
        let trace_store = ProjectCatalogStore::initialize_empty(trace_path.clone()).unwrap();
        let old = state_fingerprint(&trace_store.snapshot().unwrap());
        drop(trace_store);
        let recording = Arc::new(TracingIo::recording());
        let trace_store =
            ProjectCatalogStore::open_existing_with_io(trace_path, recording.clone()).unwrap();
        trace_store.transact(1, add_promoted_fixture).unwrap();
        let new = state_fingerprint(&trace_store.snapshot().unwrap());
        drop(trace_store);
        let trace = recording.trace();
        assert!(trace.contains(&FaultPoint::BackupWrite));
        assert!(trace.contains(&FaultPoint::BackupFsync));
        assert!(trace.contains(&FaultPoint::StageWrite));
        assert!(trace.contains(&FaultPoint::StageFsync));
        assert!(trace.contains(&FaultPoint::Cleanup));
        assert!(trace.contains(&FaultPoint::CompletePlanVerify));

        for index in 0..trace.len() {
            let (_directory, path) = projects_path();
            let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
            drop(store);
            let failing = Arc::new(TracingIo::failing_at(index));
            let store = ProjectCatalogStore::open_existing_with_io(path.clone(), failing).unwrap();
            let _ = store.transact(1, add_promoted_fixture);
            drop(store);
            assert_known_state_or_absent(&path, &[old.clone(), new.clone()]);
            assert_retained_journal_artifacts(&path);
        }
    }

    #[test]
    fn live_handle_reconciles_after_a_post_commit_fault() {
        let (_trace_directory, trace_path) = projects_path();
        let trace_store = ProjectCatalogStore::initialize_empty(trace_path.clone()).unwrap();
        drop(trace_store);
        let recording = Arc::new(TracingIo::recording());
        let trace_store =
            ProjectCatalogStore::open_existing_with_io(trace_path, recording.clone()).unwrap();
        trace_store.transact(1, |_, _| Ok(())).unwrap();
        drop(trace_store);
        let fail_at = recording
            .trace()
            .iter()
            .rposition(|point| *point == FaultPoint::CommittedJournalWrite)
            .unwrap();

        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let failing = Arc::new(TracingIo::failing_at(fail_at));
        let store = ProjectCatalogStore::open_existing_with_io(path, failing).unwrap();
        let error = store.transact(1, |_, _| Ok(())).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");
        assert_eq!(store.snapshot().unwrap().epoch(), 2);
        assert_eq!(store.transact(2, |_, _| Ok(())).unwrap().epoch, 3);
    }

    #[test]
    fn forward_recovery_fault_matrix_survives_a_second_crash() {
        fn leave_prepared(path: &Path) {
            let store = ProjectCatalogStore::initialize_empty(path.to_path_buf()).unwrap();
            drop(store);
            let failing = Arc::new(TracingIo::failing_points([
                FaultPoint::ParticipantInstall,
                FaultPoint::RecoveryParticipantInstall,
            ]));
            let store =
                ProjectCatalogStore::open_existing_with_io(path.to_path_buf(), failing).unwrap();
            assert!(store.transact(1, |_, _| Ok(())).is_err());
            assert_eq!(
                store.snapshot().unwrap_err().code(),
                "error.project_catalog_store_poisoned"
            );
            drop(store);
            let paths = ProjectCatalogPaths::derive(path).unwrap();
            let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(journal.state, TransactionStateV1::Prepared);
        }

        let (_reference_directory, reference_path) = projects_path();
        let reference = ProjectCatalogStore::initialize_empty(reference_path).unwrap();
        reference.transact(1, |_, _| Ok(())).unwrap();
        let expected = state_fingerprint(&reference.snapshot().unwrap());
        drop(reference);

        let (_trace_directory, trace_path) = projects_path();
        leave_prepared(&trace_path);
        let recording = Arc::new(TracingIo::recording());
        let recovered =
            ProjectCatalogStore::open_existing_with_io(trace_path, recording.clone()).unwrap();
        assert_eq!(state_fingerprint(&recovered.snapshot().unwrap()), expected);
        drop(recovered);
        let trace = recording.trace();
        assert!(trace.contains(&FaultPoint::RecoveryParticipantInstall));

        for index in 0..trace.len() {
            let (_directory, path) = projects_path();
            leave_prepared(&path);
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = ProjectCatalogStore::open_existing_with_io(path.clone(), failing);
            assert_known_state_or_absent(&path, std::slice::from_ref(&expected));
            assert_retained_journal_artifacts(&path);
        }
    }

    #[test]
    fn missing_attachment_stage_forces_exact_rollback_across_a_second_crash() {
        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let initial = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        assert!(transact_migration_with_io(&path, plan, initial).is_err());

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        let attachment_stage = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Attachments)
            .and_then(|participant| match &participant.new {
                ExpectedImageV1::Present { artifact_name, .. } => {
                    Some(paths.stage_dir.join(artifact_name.as_str()))
                }
                ExpectedImageV1::Absent {} => None,
            })
            .unwrap();
        fs::remove_file(attachment_stage).unwrap();

        let second_crash = Arc::new(TracingIo::failing_points([
            FaultPoint::RecoveryParticipantRestore,
        ]));
        assert!(recover_migration_with_io(&path, registry.clone(), second_crash).is_err());
        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();

        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert!(!paths.attachments.exists());
        let recovered: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(recovered.outcome, Some(TransactionOutcomeV1::RolledBack));
    }

    #[test]
    fn missing_legacy_source_can_roll_back_and_reapply_the_exact_plan() {
        let (_directory, path, plan, expected) = missing_source_migration_fault_fixture();
        let registry = plan.registry.clone();
        let reopen_registry = registry.clone();
        let retry = plan.clone();
        let initial = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        assert!(transact_migration_with_io(&path, plan, initial).is_err());

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        let attachment_stage = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Attachments)
            .and_then(|participant| match &participant.new {
                ExpectedImageV1::Present { artifact_name, .. } => {
                    Some(paths.stage_dir.join(artifact_name.as_str()))
                }
                ExpectedImageV1::Absent {} => None,
            })
            .unwrap();
        fs::remove_file(attachment_stage).unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_absent_pair(&path);
        let rolled_back: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(rolled_back.outcome, Some(TransactionOutcomeV1::RolledBack));

        assert_eq!(transact_migration(&path, retry).unwrap().epoch, 1);
        let reopened =
            ProjectCatalogStore::open_existing_after_migration(path, reopen_registry).unwrap();
        assert_eq!(state_fingerprint(&reopened.snapshot().unwrap()), expected);
    }

    #[test]
    fn missing_pinned_immutable_asset_does_not_block_prepared_rollback() {
        let (_directory, path, plan, _, _) = active_migration_fault_fixture();
        let registry = plan.registry.clone();
        let pinned = plan
            .journal
            .immutable_assets
            .iter()
            .find(|asset| asset.mode == ImmutableAssetModeV1::PinnedExisting)
            .unwrap();
        let pinned_target = registry.immutable_target(&pinned.role, &pinned.validated_name);
        let initial = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        assert!(transact_migration_with_io(&path, plan, initial).is_err());
        fs::remove_file(pinned_target).unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let recovered: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(recovered.outcome, Some(TransactionOutcomeV1::RolledBack));
    }

    #[test]
    fn unrecoverable_prepared_migration_is_stably_classified_for_retry() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let retry = plan.clone();
        let initial = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        assert!(transact_migration_with_io(&path, plan, initial).is_err());
        fs::write(&path, b"unexplained catalog bytes").unwrap();

        for _ in 0..2 {
            let error =
                recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo))
                    .unwrap_err();
            assert_eq!(error.code(), "error.project_catalog_recovery_incomplete");
        }
        let failure =
            transact_migration_classified_with_io(&path, retry, Arc::new(RealCatalogStoreIo))
                .unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RetryExactPlanRequired
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.state, TransactionStateV1::Prepared);
        assert_eq!(journal.outcome, None);
    }

    #[test]
    fn regular_transactions_retain_migration_identity_and_reopen_authority() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let mut expected_identity = plan.artifact_identity();
        expected_identity.migration_install_is_current = false;
        let registry = plan.registry.clone();
        transact_migration(&path, plan).unwrap();

        let store =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), registry.clone())
                .unwrap();
        assert_eq!(store.transact(1, |_, _| Ok(())).unwrap().epoch, 2);
        assert_eq!(
            store.migration_artifact_identity().unwrap(),
            expected_identity
        );
        drop(store);

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(paths.migration_receipt.is_file());
        fs::remove_dir_all(&paths.backup_dir).unwrap();

        let reopened =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), registry).unwrap();
        assert_eq!(
            reopened.migration_artifact_identity().unwrap(),
            expected_identity
        );
    }

    #[test]
    fn rollback_recovery_fault_matrix_deletes_only_the_exact_new_image() {
        let (_trace_directory, successful_path) = projects_path();
        let recording = Arc::new(TracingIo::recording());
        let initialized =
            ProjectCatalogStore::initialize_empty_with_io(successful_path, recording.clone())
                .unwrap();
        drop(initialized);
        let install_points = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::ParticipantInstall).then_some(index)
            })
            .collect::<Vec<_>>();
        let fail_after_first_install = install_points[1];

        fn leave_rollback_fixture(path: &Path, fail_after_first_install: usize) {
            let failing = Arc::new(TracingIo::failing_at_and_points(
                fail_after_first_install,
                std::iter::empty(),
            ));
            assert!(
                ProjectCatalogStore::initialize_empty_with_io(path.to_path_buf(), failing).is_err()
            );
            corrupt_staged_role(path, ParticipantRoleV1::Attachments);
        }

        let (_recovery_trace_directory, recovery_trace_path) = projects_path();
        leave_rollback_fixture(&recovery_trace_path, fail_after_first_install);
        let recovery_recording = Arc::new(TracingIo::recording());
        let error = ProjectCatalogStore::open_existing_with_io(
            recovery_trace_path,
            recovery_recording.clone(),
        )
        .unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_not_initialized");
        let recovery_trace = recovery_recording.trace();
        assert!(recovery_trace.contains(&FaultPoint::RecoveryParticipantRestore));
        assert!(recovery_trace.contains(&FaultPoint::RecoveryParticipantDelete));

        for index in 0..recovery_trace.len() {
            let (_directory, path) = projects_path();
            leave_rollback_fixture(&path, fail_after_first_install);
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = ProjectCatalogStore::open_existing_with_io(path.clone(), failing);
            let error = ProjectCatalogStore::open_existing(path.clone()).unwrap_err();
            assert_eq!(error.code(), "error.project_catalog_not_initialized");
            assert_absent_pair(&path);
            let paths = ProjectCatalogPaths::derive(&path).unwrap();
            let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));
        }
    }

    #[test]
    fn migration_journal_refuses_catalog_only_recovery_context() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let bytes = fs::read(&paths.journal).unwrap();
        let mut journal: ProjectCatalogTransactionJournalV1 =
            decode_bounded_json(&bytes, MAX_JOURNAL_BYTES, "transaction journal").unwrap();
        journal.kind = TransactionKindV1::V1Migration;
        journal.plan_hash = Some(sha256(b"synthetic migration plan"));
        let publisher_hash = sha256(b"legacy publisher refs");
        journal.publisher_ref_source = Some(MigrationPublisherSourceEvidenceV1::Present {
            sha256: publisher_hash.clone(),
        });
        let catalog = journal
            .participants
            .iter_mut()
            .find(|participant| participant.role == ParticipantRoleV1::Catalog)
            .unwrap();
        let legacy_hash = sha256(b"synthetic legacy catalog");
        catalog.old = ExpectedImageV1::Present {
            artifact_name: artifact_name(
                &journal.transaction_id,
                &ParticipantRoleV1::Catalog,
                &legacy_hash,
                ArtifactKind::Backup,
            )
            .unwrap(),
            sha256: legacy_hash.clone(),
        };
        for role in [
            ParticipantRoleV1::EffectiveSourceManifest,
            ParticipantRoleV1::MigrationMarker,
        ] {
            let hash = sha256(role.artifact_token().as_bytes());
            journal.participants.push(TransactionParticipantV1 {
                role: role.clone(),
                old: ExpectedImageV1::Absent {},
                new: ExpectedImageV1::Present {
                    sha256: hash.clone(),
                    artifact_name: artifact_name(
                        &journal.transaction_id,
                        &role,
                        &hash,
                        ArtifactKind::Stage,
                    )
                    .unwrap(),
                },
            });
        }
        journal.immutable_assets = [
            (ImmutableAssetRoleV1::LegacyProjectStoreBackup, legacy_hash),
            (
                ImmutableAssetRoleV1::LegacyPublisherRefBackup,
                publisher_hash,
            ),
        ]
        .into_iter()
        .map(|(role, hash)| ImmutableAssetV1 {
            validated_name: immutable_target_name(&journal.transaction_id, &role, &hash).unwrap(),
            stage_name: Some(immutable_stage_name(&journal.transaction_id, &role, &hash).unwrap()),
            role,
            mode: ImmutableAssetModeV1::Installable,
            sha256: hash,
        })
        .collect();
        fs::write(
            &paths.journal,
            encode_bounded_json(&journal, MAX_JOURNAL_BYTES, "transaction journal").unwrap(),
        )
        .unwrap();

        // D-029: a regular owner admits only a terminal committed migration
        // journal that validates and binds to the installed catalog origin.
        // This synthetic journal is terminal-shaped but not a valid
        // migration journal, so strict validation refuses it before the
        // origin coherence check is even reached.
        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_journal");
    }

    #[test]
    fn validated_migration_plan_uses_closed_roles_and_commits_every_participant() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let retry_plan = plan.clone();
        let retry_after_regular_commit = plan.clone();
        let reopen_registry = plan.registry.clone();
        let commit = transact_migration(&path, plan).unwrap();
        assert_eq!(commit.epoch, 1);
        let retried = transact_migration(&path, retry_plan).unwrap();
        assert_eq!(retried, commit);

        let reopened =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), reopen_registry)
                .unwrap();
        assert_eq!(reopened.snapshot().unwrap().epoch(), 1);
        assert_eq!(reopened.transact(1, |_, _| Ok(())).unwrap().epoch, 2);
        drop(reopened);
        assert_eq!(
            transact_migration(&path, retry_after_regular_commit)
                .unwrap()
                .epoch,
            2
        );
        assert_eq!(
            ProjectCatalogStore::open_existing(path.clone())
                .unwrap()
                .snapshot()
                .unwrap()
                .epoch(),
            2
        );

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let catalog = decode_catalog_snapshot(&fs::read(paths.catalog).unwrap()).unwrap();
        let attachments =
            decode_attachment_snapshot(&fs::read(paths.attachments).unwrap()).unwrap();
        validate_catalog_attachments(&catalog, &attachments).unwrap();
        let marker: ProjectCatalogMigrationMarkerV1 = decode_bounded_json(
            &fs::read(paths.migration_marker).unwrap(),
            MAX_MARKER_BYTES,
            "migration marker",
        )
        .unwrap();
        marker.validate().unwrap();
        assert!(marker.participants.iter().any(|evidence| {
            evidence.role == ParticipantRoleV1::EffectiveSourceManifest
                && matches!(evidence.new, ExpectedImageV1::Present { .. })
        }));
        assert_eq!(marker.immutable_assets.len(), 1);
        assert_eq!(
            marker.publisher_ref_source,
            MigrationPublisherSourceEvidenceV1::missing()
        );
        assert!(
            !marker
                .immutable_assets
                .iter()
                .any(|asset| asset.role == ImmutableAssetRoleV1::LegacyPublisherRefBackup)
        );
        let root = path.parent().unwrap();
        assert_eq!(
            fs::read_to_string(root.join("checkout/.bbox/local/checkout-id")).unwrap(),
            "66666666666666666666666666666666"
        );
        assert_eq!(
            fs::read(root.join("checkout/.bbox/local/.gitignore")).unwrap(),
            CHECKOUT_LOCAL_GITIGNORE_BYTES
        );
        assert_eq!(
            bbox_corpus_core::identity::ensure_checkout_id(&root.join("checkout")).unwrap(),
            "66666666666666666666666666666666"
        );
    }

    #[test]
    fn completed_plan_retry_refuses_same_plan_with_different_report_bytes() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, draft, _) = basic_migration_draft(&path, legacy);
        let mut changed = draft.clone();
        changed.report_artifact_sha256 = Sha256Hex::digest(b"different reviewed report bytes");
        let original = validate_migration_plan(&path, registry.clone(), draft).unwrap();
        let retry = validate_migration_plan(&path, registry, changed).unwrap();

        transact_migration(&path, original).unwrap();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let marker_before = fs::read(&paths.migration_marker).unwrap();
        let error = transact_migration(&path, retry).unwrap_err();

        assert_eq!(error.code(), "error.project_catalog_migration_incomplete");
        assert_eq!(fs::read(paths.migration_marker).unwrap(), marker_before);
    }

    #[test]
    fn completed_plan_retry_refuses_same_plan_with_different_resolution_bytes() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, draft, _) = basic_migration_draft(&path, legacy);
        let mut changed = draft.clone();
        changed.resolution_artifact_sha256 =
            Sha256Hex::digest(b"different reviewed resolution bytes");
        let original = validate_migration_plan(&path, registry.clone(), draft).unwrap();
        let retry = validate_migration_plan(&path, registry, changed).unwrap();

        transact_migration(&path, original).unwrap();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let marker_before = fs::read(&paths.migration_marker).unwrap();
        let error = transact_migration(&path, retry).unwrap_err();

        assert_eq!(error.code(), "error.project_catalog_migration_incomplete");
        assert_eq!(fs::read(paths.migration_marker).unwrap(), marker_before);
    }

    #[test]
    fn fresh_migration_identity_requires_exact_committed_marker_and_journal() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let expected = plan.artifact_identity();
        let registry = plan.registry.clone();
        transact_migration(&path, plan).unwrap();

        let store =
            ProjectCatalogStore::open_existing_after_migration(path.clone(), registry.clone())
                .unwrap();
        assert_eq!(store.migration_artifact_identity().unwrap(), expected);
        drop(store);

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let mut journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        journal.report_artifact_sha256 = Some(Sha256Hex::digest(b"mismatched report identity"));
        fs::write(
            &paths.journal,
            encode_bounded_json(&journal, MAX_JOURNAL_BYTES, "transaction journal").unwrap(),
        )
        .unwrap();

        let error = ProjectCatalogStore::open_existing_after_migration(path, registry).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_migration_incomplete");
    }

    #[test]
    fn recovery_refuses_forward_when_reviewed_artifact_identity_disagrees() {
        let (_directory, path, plan, _, legacy) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        assert!(transact_migration_with_io(&path, plan, failing).is_err());

        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let mut journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.state, TransactionStateV1::Prepared);
        journal.resolution_artifact_sha256 =
            Some(Sha256Hex::digest(b"mismatched recovery resolution"));
        fs::write(
            &paths.journal,
            encode_bounded_json(&journal, MAX_JOURNAL_BYTES, "transaction journal").unwrap(),
        )
        .unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy);
        assert!(!paths.attachments.exists());
        let recovered: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(recovered.outcome, Some(TransactionOutcomeV1::RolledBack));
    }

    #[test]
    fn completed_plan_retry_accepts_a_strict_later_code_source_generation() {
        let (_directory, path, plan, _, _) = active_migration_fault_fixture();
        let retry = plan.clone();
        let registry = plan.registry.clone();
        transact_migration(&path, plan).unwrap();

        let project_id = ProjectId::parse("active-project").unwrap();
        let scope = PublishedScope::try_new("active-repo", ".").unwrap();
        let entries = Vec::<bbox_code_source::ManifestEntry>::new();
        let head_commit = "9".repeat(40);
        let descriptor = bbox_code_source::GenerationDescriptor {
            schema_version: bbox_code_source::SCHEMA_VERSION,
            walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.into(),
            scope: scope.clone(),
            head_commit: head_commit.clone(),
            dirty_fingerprint: bbox_code_source::dirty_fingerprint(&head_commit, &entries),
            manifest_sha256: bbox_code_source::manifest_sha256(&entries),
            file_count: 0,
            logical_bytes: 0,
        };
        let producer_id = "later-producer";
        let generation_id = bbox_code_source::generation_id(producer_id, &descriptor);
        let selector = historical_selector(project_id.as_str(), &generation_id);
        let stored = bbox_code_source_store::StoredGenerationV2 {
            version: 2,
            generation_id: generation_id.clone(),
            producer_id: producer_id.into(),
            ordinal: 2,
            descriptor,
            published_scope: scope.clone(),
            state: bbox_code_source::GenerationState::Active,
            diagnostic: None,
            created_unix_secs: 2,
            materialized_doc_count: Some(0),
            entity_inventory_sha256: Some("8".repeat(64)),
        };
        let activation = bbox_code_source_store::ActivationRecordV2 {
            version: 2,
            project_id: project_id.clone(),
            published_scope: scope.clone(),
            generation_id: generation_id.clone(),
            selector: selector.clone(),
            snapshot_id: format!("collected-{}", "7".repeat(32)),
            document_count: 0,
            entity_inventory_sha256: "8".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 2,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        };
        let metadata = registry
            .code_source_paths
            .generation_metadata(&scope, &generation_id)
            .unwrap();
        fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        fs::write(
            &metadata,
            bbox_code_source_store::encode_stored_generation_v2_for_migration(&stored).unwrap(),
        )
        .unwrap();
        fs::write(
            registry
                .code_source_paths
                .generation_manifest(&scope, &generation_id)
                .unwrap(),
            b"",
        )
        .unwrap();
        fs::write(
            registry.code_source_paths.activation(&project_id),
            bbox_code_source_store::encode_activation_v2_for_migration(&activation).unwrap(),
        )
        .unwrap();
        let effective = MigrationEffectiveSourceManifestV1 {
            version: 1,
            selections: vec![
                bbox_code_source_store::MigrationEffectiveSourceSelectionV1 {
                    project_id,
                    published_scope: scope,
                    generation_id,
                    selector,
                },
            ],
        };
        fs::write(
            registry.code_source_paths.anchor(),
            bbox_code_source_store::encode_migration_effective_source_manifest_v1(&effective)
                .unwrap(),
        )
        .unwrap();

        assert_eq!(transact_migration(&path, retry).unwrap().epoch, 1);
    }

    #[test]
    fn completed_plan_retry_rejects_corrupt_current_code_source_state() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let retry = plan.clone();
        let registry = plan.registry.clone();
        transact_migration(&path, plan).unwrap();
        fs::write(registry.code_source_paths.anchor(), b"{}").unwrap();
        let error = transact_migration(&path, retry).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_migration_incomplete");
    }

    #[test]
    fn migration_marker_uses_its_role_cap_during_install_verify_and_rollback() {
        let (_directory, path) = projects_path();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let registry = MigrationParticipantRegistry::new(
            &path,
            path.parent().unwrap().join("code-source"),
            path.parent().unwrap().join("publisher-refs.json"),
            StoreLimits::default(),
        )
        .unwrap();
        let owner = ProjectCatalogTransactionOwner {
            paths: paths.clone(),
            registry: ParticipantRegistry::Migration(Arc::new(registry)),
            io: Arc::new(RealCatalogStoreIo),
        };
        fs::create_dir_all(&paths.stage_dir).unwrap();

        let transaction_id = ProjectCatalogTransactionId::mint();
        let source_hash = Sha256Hex::digest(b"legacy source");
        let mut evidence = vec![
            build_transaction_participant(
                &transaction_id,
                ParticipantRoleV1::Catalog,
                Some(source_hash.clone()),
                &Some(b"catalog".to_vec()),
            )
            .unwrap(),
            build_transaction_participant(
                &transaction_id,
                ParticipantRoleV1::Attachments,
                None,
                &Some(b"attachments".to_vec()),
            )
            .unwrap(),
            build_transaction_participant(
                &transaction_id,
                ParticipantRoleV1::EffectiveSourceManifest,
                None,
                &Some(b"manifest".to_vec()),
            )
            .unwrap(),
        ]
        .into_iter()
        .map(|participant| MigrationParticipantEvidenceV1 {
            role: participant.role,
            old: participant.old,
            new: participant.new,
        })
        .collect::<Vec<_>>();
        let long_scope = PublishedScope::try_new(
            "r".repeat(256),
            std::iter::repeat_n("p".repeat(255), 15)
                .collect::<Vec<_>>()
                .join("/"),
        )
        .unwrap();
        let project_id = ProjectId::parse("large-marker-project").unwrap();
        for ordinal in 0_u32..2_500 {
            let participant = build_transaction_participant(
                &transaction_id,
                ParticipantRoleV1::StoredGenerationMetadata {
                    project_id: project_id.clone(),
                    published_scope: long_scope.clone(),
                    generation_id: Sha256Hex::digest(&ordinal.to_le_bytes()),
                },
                Some(Sha256Hex::digest(&ordinal.to_le_bytes())),
                &Some(vec![b'x']),
            )
            .unwrap();
            evidence.push(MigrationParticipantEvidenceV1 {
                role: participant.role,
                old: participant.old,
                new: participant.new,
            });
        }
        let immutable_assets = [
            ImmutableAssetRoleV1::LegacyProjectStoreBackup,
            ImmutableAssetRoleV1::LegacyPublisherRefBackup,
        ]
        .into_iter()
        .map(|role| {
            let hash = if role == ImmutableAssetRoleV1::LegacyProjectStoreBackup {
                source_hash.clone()
            } else {
                Sha256Hex::digest(b"publisher refs")
            };
            MigrationImmutableAssetEvidenceV1 {
                validated_name: immutable_target_name(&transaction_id, &role, &hash).unwrap(),
                role,
                mode: ImmutableAssetModeV1::Installable,
                sha256: hash,
            }
        })
        .collect();
        let marker = ProjectCatalogMigrationMarkerV1 {
            version: MIGRATION_MARKER_VERSION,
            transaction_id: transaction_id.clone(),
            plan_hash: Sha256Hex::digest(b"plan"),
            report_artifact_sha256: Sha256Hex::digest(b"report"),
            resolution_artifact_sha256: Sha256Hex::digest(b"resolution"),
            legacy_project_source: MigrationLegacyProjectSourceEvidenceV1::Present {
                sha256: source_hash,
            },
            publisher_ref_source: MigrationPublisherSourceEvidenceV1::Present {
                sha256: Sha256Hex::digest(b"publisher refs"),
            },
            inventory_sha256: Sha256Hex::digest(b"inventory"),
            publisher_pins: Vec::new(),
            publisher_dispositions: Vec::new(),
            participants: evidence,
            immutable_assets,
            migration_epoch: 1,
        };
        marker.validate().unwrap();
        let marker_bytes =
            encode_bounded_json(&marker, MAX_MARKER_BYTES, "migration marker").unwrap();
        assert!(marker_bytes.len() > MAX_PROJECT_CATALOG_BYTES);
        assert!(marker_bytes.len() < MAX_MARKER_BYTES);

        let marker_hash = sha256(&marker_bytes);
        let stage_name = artifact_name(
            &transaction_id,
            &ParticipantRoleV1::MigrationMarker,
            &marker_hash,
            ArtifactKind::Stage,
        )
        .unwrap();
        fs::write(paths.stage_dir.join(stage_name.as_str()), &marker_bytes).unwrap();
        let participant = TransactionParticipantV1 {
            role: ParticipantRoleV1::MigrationMarker,
            old: ExpectedImageV1::Absent {},
            new: ExpectedImageV1::Present {
                sha256: marker_hash,
                artifact_name: stage_name,
            },
        };
        let journal = ProjectCatalogTransactionJournalV1 {
            version: JOURNAL_VERSION,
            transaction_id,
            kind: TransactionKindV1::V1Migration,
            state: TransactionStateV1::Prepared,
            outcome: None,
            plan_hash: Some(Sha256Hex::digest(b"plan")),
            report_artifact_sha256: Some(Sha256Hex::digest(b"report")),
            resolution_artifact_sha256: Some(Sha256Hex::digest(b"resolution")),
            legacy_project_source: Some(MigrationLegacyProjectSourceEvidenceV1::Present {
                sha256: Sha256Hex::digest(b"legacy source"),
            }),
            publisher_ref_source: Some(MigrationPublisherSourceEvidenceV1::Present {
                sha256: Sha256Hex::digest(b"publisher refs"),
            }),
            publisher_pins: Vec::new(),
            publisher_dispositions: Vec::new(),
            resolved_quarantine_bindings: Some(BTreeSet::new()),
            old_epoch: 0,
            new_epoch: 1,
            participants: vec![participant.clone()],
            immutable_assets: Vec::new(),
            monotonic_checkout_identity_actions: Vec::new(),
            created_at: 1,
            committed_at: None,
        };

        owner.install_new_image(&participant).unwrap();
        owner
            .verify_expected_pair(&journal, ExpectedSide::New)
            .unwrap();
        assert_eq!(
            owner.classify_recovery(&journal, false).unwrap(),
            RecoveryDecision::Forward
        );
        owner.restore_old_image(&participant).unwrap();
        owner
            .verify_expected_pair(&journal, ExpectedSide::Old)
            .unwrap();
    }

    #[test]
    fn stale_plan_writes_no_assets_and_a_corrected_plan_succeeds() {
        let (_directory, path) = projects_path();
        let original = b"{\"version\":1,\"projects\":[]}\n";
        let changed = b"{\"version\":1,\"projects\":[],\"updated\":true}\n";
        fs::write(&path, original).unwrap();
        let (registry, draft, _) = basic_migration_draft(&path, original);
        let stale = validate_migration_plan(&path, registry, draft).unwrap();

        fs::write(&path, changed).unwrap();
        let error = transact_migration(&path, stale).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_migration_inventory_stale"
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());
        assert!(!paths.attachments.exists());
        assert_eq!(fs::read(&path).unwrap(), changed);

        let (registry, draft, _) = basic_migration_draft(&path, changed);
        let corrected = validate_migration_plan(&path, registry, draft).unwrap();
        assert_eq!(transact_migration(&path, corrected).unwrap().epoch, 1);
    }

    #[test]
    fn classified_stale_plan_reports_no_durable_mutation() {
        let (_directory, path) = projects_path();
        let original = b"{\"version\":1,\"projects\":[]}\n";
        let changed = b"{\"version\":1,\"projects\":[],\"updated\":true}\n";
        fs::write(&path, original).unwrap();
        let (registry, draft, _) = basic_migration_draft(&path, original);
        let stale = validate_migration_plan(&path, registry, draft).unwrap();
        fs::write(&path, changed).unwrap();

        let failure = transact_migration_classified(&path, stale).unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::NoDurableMutation
        );
        assert_eq!(
            failure.error.code(),
            "error.project_catalog_migration_inventory_stale"
        );
    }

    #[test]
    fn classified_post_prepare_fault_reports_recovered_committed_state() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let failure = transact_migration_classified_with_io(
            &path,
            plan,
            Arc::new(TracingIo::failing_at(fail_after_prepared)),
        )
        .unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RecoveredToCommittedState
        );
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(ProjectCatalogPaths::derive(&path).unwrap().journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.outcome, Some(TransactionOutcomeV1::Committed));
    }

    #[test]
    fn classified_changed_source_after_prepared_reports_recovered_old_state() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let retry = plan.clone();
        let publisher_source = plan.registry.legacy_publisher_ref_source.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        fs::write(&publisher_source, b"new publisher source").unwrap();

        let failure =
            transact_migration_classified_with_io(&path, retry, Arc::new(RealCatalogStoreIo))
                .unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RecoveredToOldState
        );
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert_eq!(fs::read(publisher_source).unwrap(), b"new publisher source");
    }

    #[test]
    fn classified_incomplete_recovery_reports_exact_plan_retry_required() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let failure = transact_migration_classified_with_io(
            &path,
            plan,
            Arc::new(TracingIo::failing_at_and_points(
                fail_after_prepared,
                [FaultPoint::RecoveryParticipantInstall],
            )),
        )
        .unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RetryExactPlanRequired
        );
    }

    #[test]
    fn classified_open_reports_no_mutation_before_recovery_entry() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let other_path = path.with_file_name("other-projects.json");

        let failure = ProjectCatalogStore::open_existing_after_migration_classified(
            other_path,
            plan.registry,
        )
        .unwrap_err();

        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::NoDurableMutation
        );
        assert_eq!(
            failure.error.code(),
            "error.project_catalog_invalid_migration_registry"
        );
    }

    #[test]
    fn classified_open_reports_prepared_forward_recovery() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );

        let opened = ProjectCatalogStore::open_existing_after_migration_classified_with_io(
            path,
            ParticipantRegistry::Migration(Arc::new(registry)),
            Arc::new(RealCatalogStoreIo),
        )
        .unwrap();

        assert_eq!(
            opened.disposition,
            MigrationMutationDispositionV1::RecoveredToCommittedState
        );
        assert_eq!(opened.store.snapshot().unwrap().epoch, 1);
    }

    #[test]
    fn classified_open_reports_prepared_rollback_recovery() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let publisher_source = registry.legacy_publisher_ref_source.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        fs::write(&publisher_source, b"new publisher source").unwrap();

        let failure = ProjectCatalogStore::open_existing_after_migration_classified_with_io(
            path.clone(),
            ParticipantRegistry::Migration(Arc::new(registry)),
            Arc::new(RealCatalogStoreIo),
        )
        .unwrap_err();

        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RecoveredToOldState
        );
        assert_eq!(fs::read(path).unwrap(), legacy_bytes);
    }

    #[test]
    fn classified_open_reports_uncertain_recovery_as_retry_required() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );

        let failure = ProjectCatalogStore::open_existing_after_migration_classified_with_io(
            path,
            ParticipantRegistry::Migration(Arc::new(registry)),
            Arc::new(TracingIo::failing_points([
                FaultPoint::RecoveryParticipantInstall,
            ])),
        )
        .unwrap_err();

        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RetryExactPlanRequired
        );
    }

    #[test]
    fn bootstrap_failure_preserves_terminal_rollback_disposition() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let retry = plan.clone();
        let publisher_source = plan.registry.legacy_publisher_ref_source.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        fs::write(&publisher_source, b"new publisher source").unwrap();
        let failure =
            transact_migration_classified_with_io(&path, retry, Arc::new(RealCatalogStoreIo))
                .unwrap_err();
        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RecoveredToOldState
        );

        let bootstrap = begin_migration_checkout_registry_bootstrap(&path).unwrap();
        assert!(matches!(
            bootstrap,
            MigrationCheckoutRegistryBootstrapV1::RolledBackNotInstalled {
                disposition: MigrationMutationDispositionV1::RecoveredToOldState
            }
        ));
    }

    #[test]
    fn bootstrap_failure_preserves_terminal_committed_disposition() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let mut bootstrap_registry = plan.registry.clone();
        bootstrap_registry.checkout_identity_markers.clear();
        transact_migration(&path, plan).unwrap();

        let bootstrap = begin_migration_checkout_registry_bootstrap(&path).unwrap();
        let MigrationCheckoutRegistryBootstrapV1::RequiresRegistry(session) = bootstrap else {
            panic!("committed migration requires its participant registry")
        };
        let session = session.bind_registry(bootstrap_registry).unwrap();
        let bootstrap_failure = session.finish_open(&BTreeMap::new()).unwrap_err();

        assert_eq!(
            bootstrap_failure.disposition,
            MigrationMutationDispositionV1::RecoveredToCommittedState
        );
        assert_eq!(
            bootstrap_failure.error.code(),
            "error.project_catalog_invalid_migration_registry"
        );
    }

    #[test]
    fn bootstrap_session_open_failure_cannot_downgrade_terminal_state() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let mut bootstrap_registry = plan.registry.clone();
        bootstrap_registry.checkout_identity_markers.clear();
        let checkout_bindings = plan
            .registry
            .checkout_identity_markers
            .iter()
            .map(|(observation_id, target)| {
                (
                    observation_id.clone(),
                    target
                        .parent()
                        .and_then(Path::parent)
                        .and_then(Path::parent)
                        .unwrap()
                        .to_path_buf(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        transact_migration(&path, plan).unwrap();

        let bootstrap = begin_migration_checkout_registry_bootstrap_with_io(
            &path,
            Arc::new(TracingIo::failing_reads([paths.migration_marker])),
        )
        .unwrap();
        let MigrationCheckoutRegistryBootstrapV1::RequiresRegistry(session) = bootstrap else {
            panic!("committed migration requires its participant registry")
        };
        assert_eq!(
            session.disposition(),
            MigrationMutationDispositionV1::RecoveredToCommittedState
        );
        let session = session.bind_registry(bootstrap_registry).unwrap();

        let failure = session.finish_open(&checkout_bindings).unwrap_err();

        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::RecoveredToCommittedState
        );
    }

    #[test]
    fn artifact_read_failure_cannot_classify_as_no_mutation() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let unreadable_artifact = plan
            .journal
            .participants
            .iter()
            .find_map(|participant| match &participant.new {
                ExpectedImageV1::Present { artifact_name, .. } => {
                    Some(paths.stage_dir.join(artifact_name.as_str()))
                }
                ExpectedImageV1::Absent {} => None,
            })
            .unwrap();

        let disposition = classify_migration_failure(
            &path,
            &plan,
            Arc::new(TracingIo::failing_reads([unreadable_artifact])),
        );

        assert_eq!(
            disposition,
            MigrationMutationDispositionV1::RetryExactPlanRequired
        );
    }

    #[test]
    fn pre_journal_failure_removes_exact_orphan_artifacts() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let paths = ProjectCatalogPaths::derive(&path).unwrap();

        let failure = transact_migration_classified_with_io(
            &path,
            plan.clone(),
            Arc::new(TracingIo::failing_points([FaultPoint::ImmutableAssetWrite])),
        )
        .unwrap_err();

        assert_eq!(
            failure.disposition,
            MigrationMutationDispositionV1::NoDurableMutation
        );
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());
        for participant in &plan.journal.participants {
            for (root, image) in [
                (&paths.stage_dir, &participant.new),
                (&paths.backup_dir, &participant.old),
            ] {
                if let ExpectedImageV1::Present { artifact_name, .. } = image {
                    assert!(!root.join(artifact_name.as_str()).exists());
                }
            }
        }
        for asset in &plan.journal.immutable_assets {
            assert!(
                !plan
                    .registry
                    .immutable_target(&asset.role, &asset.validated_name)
                    .exists()
            );
        }
    }

    #[test]
    fn changed_publisher_source_refuses_before_transaction_side_effects() {
        let (_directory, path) = projects_path();
        let legacy = b"{\"version\":1,\"projects\":[]}\n";
        fs::write(&path, legacy).unwrap();
        let (registry, draft, _) = basic_migration_draft(&path, legacy);
        let publisher_source = registry.legacy_publisher_ref_source.clone();
        let plan = validate_migration_plan(&path, registry, draft).unwrap();
        fs::write(&publisher_source, b"changed publisher refs").unwrap();

        let error = transact_migration(&path, plan).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_migration_inventory_stale"
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.stage_dir.exists());
        assert!(!paths.backup_dir.exists());
        assert!(!paths.journal.exists());
        assert!(!paths.attachments.exists());
        assert_eq!(fs::read(path).unwrap(), legacy);
    }

    #[test]
    fn prepared_recovery_rolls_back_when_missing_publisher_source_appears() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let publisher_source = registry.legacy_publisher_ref_source.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        fs::write(&publisher_source, b"new publisher source").unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert_eq!(fs::read(publisher_source).unwrap(), b"new publisher source");
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));
    }

    #[test]
    fn prepared_recovery_rolls_back_when_present_publisher_source_changes() {
        let make_plan = |path: &Path, legacy: &[u8]| {
            let (registry, draft, _, _) = publisher_seed_migration_draft(path, legacy);
            validate_migration_plan(path, registry, draft).unwrap()
        };
        let (_trace_directory, trace_path) = projects_path();
        let legacy_bytes = b"{\"version\":1,\"projects\":[]}\n".to_vec();
        fs::write(&trace_path, &legacy_bytes).unwrap();
        let trace_plan = make_plan(&trace_path, &legacy_bytes);
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path) = projects_path();
        fs::write(&path, &legacy_bytes).unwrap();
        let plan = make_plan(&path, &legacy_bytes);
        let registry = plan.registry.clone();
        let publisher_source = registry.legacy_publisher_ref_source.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        fs::write(&publisher_source, b"changed publisher source").unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert_eq!(
            fs::read(publisher_source).unwrap(),
            b"changed publisher source"
        );
    }

    #[test]
    fn prepared_recovery_rolls_back_on_an_unexplained_code_source_row() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        assert!(
            transact_migration_with_io(
                &path,
                plan,
                Arc::new(TracingIo::failing_at(fail_after_prepared)),
            )
            .is_err()
        );
        write_unprotected_legacy_generation(&registry.code_source_paths);

        recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        let inventory = {
            let guard = registry
                .code_source_paths
                .lock_migration_inventory()
                .unwrap();
            guard.snapshot_legacy_v1(&StoreLimits::default()).unwrap()
        };
        assert_eq!(inventory.generation_count, 1);
        assert_eq!(inventory.unprotected_generation_count, 1);
        assert!(inventory.generations.is_empty());
    }

    #[test]
    fn same_plan_retry_resyncs_preexisting_artifacts() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let retry = plan.clone();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::DirectoryFsync]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");

        let recording = Arc::new(TracingIo::recording());
        assert_eq!(
            transact_migration_with_io(&path, retry, recording.clone())
                .unwrap()
                .epoch,
            1
        );
        assert!(recording.trace().contains(&FaultPoint::BackupFsync));
        assert!(recording.trace().contains(&FaultPoint::DirectoryFsync));
    }

    #[test]
    fn migration_recovery_recreates_missing_participant_parents() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_points([FaultPoint::ParticipantInstall]));
        let error = transact_migration_with_io(&path, plan, failing).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_injected_fault");

        let code_source_root = path.parent().unwrap().join("code-source");
        if code_source_root.exists() {
            fs::remove_dir_all(&code_source_root).unwrap();
        }
        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        let expected_effective =
            bbox_code_source_store::encode_migration_effective_source_manifest_v1(
                &MigrationEffectiveSourceManifestV1 {
                    version: 1,
                    selections: Vec::new(),
                },
            )
            .unwrap();
        assert_eq!(
            fs::read(code_source_root.join("effective-source-manifest.json")).unwrap(),
            expected_effective
        );
    }

    #[test]
    fn prepared_migration_rolls_back_around_a_new_valid_checkout_id() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_at(fail_after_prepared));
        assert!(transact_migration_with_io(&path, plan, failing).is_err());
        let checkout_id = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::create_dir_all(checkout_id.parent().unwrap()).unwrap();
        fs::write(&checkout_id, b"99999999999999999999999999999999\n").unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
        assert_eq!(
            fs::read(&checkout_id).unwrap(),
            b"99999999999999999999999999999999\n"
        );
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(!paths.attachments.exists());
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        assert_eq!(journal.state, TransactionStateV1::Committed);
        assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));

        let changed = b"{\"version\":1,\"projects\":[],\"updated\":true}\n";
        fs::write(&path, changed).unwrap();
        let (registry, mut draft, _) = basic_migration_draft(&path, changed);
        draft
            .attachments
            .attachments
            .values_mut()
            .next()
            .unwrap()
            .checkout_id = "99999999999999999999999999999999".into();
        draft.checkout_identity_actions.clear();
        let corrected = validate_migration_plan(&path, registry, draft).unwrap();
        assert_eq!(transact_migration(&path, corrected).unwrap().epoch, 1);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        assert!(fs::read_dir(paths.backup_dir).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".rollback-journal.")
        }));
    }

    #[test]
    fn malformed_or_unavailable_checkout_state_does_not_block_prepared_rollback() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_prepared = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::PreparedJournalWrite).then_some(index)
            })
            .nth(1)
            .unwrap();

        for unavailable in [false, true] {
            let (_directory, path, plan, _, legacy_bytes) = migration_fault_fixture();
            let registry = plan.registry.clone();
            assert!(
                transact_migration_with_io(
                    &path,
                    plan,
                    Arc::new(TracingIo::failing_at(fail_after_prepared)),
                )
                .is_err()
            );
            let checkout_root = path.parent().unwrap().join("checkout");
            fs::create_dir_all(&checkout_root).unwrap();
            if unavailable {
                let bbox = checkout_root.join(".bbox");
                if bbox.exists() {
                    fs::remove_dir_all(&bbox).unwrap();
                }
                fs::write(bbox, b"not a directory").unwrap();
            } else {
                let checkout_local = checkout_root.join(".bbox/local");
                fs::create_dir_all(&checkout_local).unwrap();
                fs::write(checkout_local.join("checkout-id"), b"malformed\n").unwrap();
            }

            recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
            assert_eq!(fs::read(&path).unwrap(), legacy_bytes);
            let paths = ProjectCatalogPaths::derive(&path).unwrap();
            assert!(!paths.attachments.exists());
            let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
                &fs::read(paths.journal).unwrap(),
                MAX_JOURNAL_BYTES,
                "transaction journal",
            )
            .unwrap();
            assert_eq!(journal.outcome, Some(TransactionOutcomeV1::RolledBack));
        }
    }

    #[test]
    fn committed_migration_does_not_reassert_a_replaced_checkout_id() {
        let (_directory, path, plan, _, _) = migration_fault_fixture();
        let registry = plan.registry.clone();
        transact_migration(&path, plan).unwrap();
        let checkout_id = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::write(&checkout_id, b"99999999999999999999999999999999\n").unwrap();

        recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
        assert_eq!(
            fs::read(checkout_id).unwrap(),
            b"99999999999999999999999999999999\n"
        );
    }

    #[test]
    fn conflicting_checkout_id_never_forces_forward_without_rollback_evidence() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let fail_after_catalog_install = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::ParticipantInstall).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_directory, path, plan, _, _legacy_bytes) = migration_fault_fixture();
        let registry = plan.registry.clone();
        let failing = Arc::new(TracingIo::failing_at(fail_after_catalog_install));
        assert!(transact_migration_with_io(&path, plan, failing).is_err());
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        let journal: ProjectCatalogTransactionJournalV1 = decode_bounded_json(
            &fs::read(&paths.journal).unwrap(),
            MAX_JOURNAL_BYTES,
            "transaction journal",
        )
        .unwrap();
        let catalog_backup = journal
            .participants
            .iter()
            .find(|participant| participant.role == ParticipantRoleV1::Catalog)
            .and_then(|participant| match &participant.old {
                ExpectedImageV1::Present { artifact_name, .. } => {
                    Some(paths.backup_dir.join(artifact_name.as_str()))
                }
                ExpectedImageV1::Absent {} => None,
            })
            .unwrap();
        fs::remove_file(catalog_backup).unwrap();
        let checkout_id = path
            .parent()
            .unwrap()
            .join("checkout/.bbox/local/checkout-id");
        fs::write(&checkout_id, b"99999999999999999999999999999999\n").unwrap();
        let catalog_before_recovery = fs::read(&path).unwrap();

        let error =
            recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_recovery_incomplete");
        assert_eq!(fs::read(&path).unwrap(), catalog_before_recovery);
        assert!(!paths.attachments.exists());
        assert_eq!(
            fs::read(checkout_id).unwrap(),
            b"99999999999999999999999999999999\n"
        );
    }

    #[test]
    fn migration_fault_matrix_preserves_exact_old_or_new_state() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let trace = recording.trace();
        for required in [
            FaultPoint::ImmutableAssetWrite,
            FaultPoint::ImmutableAssetFsync,
            FaultPoint::PreparedJournalWrite,
            FaultPoint::MonotonicCheckoutIdentityAction,
            FaultPoint::ParticipantInstall,
            FaultPoint::ImmutableAssetVerify,
            FaultPoint::CompletePlanVerify,
            FaultPoint::CommittedJournalWrite,
        ] {
            assert!(
                trace.contains(&required),
                "missing migration point {required:?}"
            );
        }

        for index in 0..trace.len() {
            let (_directory, path, plan, expected_new, legacy_bytes) = migration_fault_fixture();
            let registry = plan.registry.clone();
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = transact_migration_with_io(&path, plan, failing);
            recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
            let paths = ProjectCatalogPaths::derive(&path).unwrap();
            let catalog_bytes = fs::read(&paths.catalog).unwrap();
            if decode_legacy_project_store(&catalog_bytes).is_ok() {
                assert_eq!(catalog_bytes, legacy_bytes);
                assert!(!paths.attachments.exists());
            } else {
                let catalog = decode_catalog_snapshot(&catalog_bytes).unwrap();
                let attachment_bytes = fs::read(paths.attachments).unwrap();
                let attachments = decode_attachment_snapshot(&attachment_bytes).unwrap();
                validate_catalog_attachments(&catalog, &attachments).unwrap();
                let actual = (
                    catalog.epoch,
                    sha256(&catalog_bytes).to_string(),
                    sha256(&attachment_bytes).to_string(),
                );
                assert_eq!(actual, expected_new);
            }
        }
    }

    #[test]
    fn migration_extended_fault_matrix_covers_pointer_and_both_asset_modes() {
        let (_trace_directory, trace_path, trace_plan, _, _) = extended_migration_fault_fixture();
        assert!(trace_plan.journal.participants.iter().any(|participant| {
            matches!(
                &participant.role,
                ParticipantRoleV1::AcceptedPublicationPointer { .. }
            )
        }));
        assert!(
            trace_plan
                .journal
                .immutable_assets
                .iter()
                .any(|asset| { asset.mode == ImmutableAssetModeV1::PinnedExisting })
        );
        assert!(
            trace_plan
                .journal
                .immutable_assets
                .iter()
                .any(|asset| { asset.mode == ImmutableAssetModeV1::Installable })
        );
        assert!(
            trace_plan
                .code_source_snapshot
                .generations
                .iter()
                .any(|generation| {
                    generation.disposition == MigrationCodeSourceDispositionV1::SurvivingRetained
                })
        );
        assert!(
            trace_plan
                .code_source_snapshot
                .generations
                .iter()
                .any(|generation| {
                    generation.disposition == MigrationCodeSourceDispositionV1::QuarantinedCollision
                })
        );
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let trace = recording.trace();
        assert!(trace.contains(&FaultPoint::ImmutableAssetWrite));
        assert!(trace.contains(&FaultPoint::ImmutableAssetVerify));

        for index in 0..trace.len() {
            let (_directory, path, plan, manifest_target, manifest_bytes) =
                extended_migration_fault_fixture();
            let registry = plan.registry.clone();
            let accepted_paths = registry.accepted_publication_paths.clone();
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = transact_migration_with_io(&path, plan, failing);
            recover_migration_with_io(&path, registry, Arc::new(RealCatalogStoreIo)).unwrap();
            assert_eq!(fs::read(&manifest_target).unwrap(), manifest_bytes);
            let catalog_bytes = fs::read(&path).unwrap();
            if decode_legacy_project_store(&catalog_bytes).is_ok() {
                assert!(
                    fs::read_dir(accepted_paths.pointers())
                        .map(|mut entries| entries.next().is_none())
                        .unwrap_or(true)
                );
            } else {
                assert!(
                    accepted_paths
                        .pointer(&ProjectId::parse("published-project").unwrap())
                        .exists()
                );
            }
        }
    }

    #[test]
    fn migration_active_source_fault_matrix_preserves_typed_source_state() {
        let (_trace_directory, trace_path, trace_plan, _, _) = active_migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let trace = recording.trace();

        for index in 0..trace.len() {
            let (_directory, path, plan, activation_role, stored_role) =
                active_migration_fault_fixture();
            let registry = plan.registry.clone();
            let failing = Arc::new(TracingIo::failing_at(index));
            let _ = transact_migration_with_io(&path, plan, failing);
            recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo))
                .unwrap();
            let activation = fs::read(
                registry
                    .participant_target(&activation_role)
                    .expect("registered activation"),
            )
            .unwrap();
            let stored = fs::read(
                registry
                    .participant_target(&stored_role)
                    .expect("registered stored generation"),
            )
            .unwrap();
            let catalog_bytes = fs::read(&path).unwrap();
            if decode_legacy_project_store(&catalog_bytes).is_ok() {
                decode_activation_v1_for_migration(&activation).unwrap();
                decode_stored_generation_v1_for_migration(&stored).unwrap();
            } else {
                decode_activation_v2_for_migration(&activation).unwrap();
                decode_stored_generation_v2_for_migration(&stored).unwrap();
            }
        }
    }

    #[test]
    fn migration_recovery_fault_matrix_is_repeatable() {
        let (_trace_directory, trace_path, trace_plan, _, _) = migration_fault_fixture();
        let recording = Arc::new(TracingIo::recording());
        transact_migration_with_io(&trace_path, trace_plan, recording.clone()).unwrap();
        let initial_failure = recording
            .trace()
            .iter()
            .enumerate()
            .filter_map(|(index, point)| {
                (*point == FaultPoint::ParticipantInstall).then_some(index)
            })
            .nth(1)
            .unwrap();

        let (_recovery_directory, recovery_path, recovery_plan, _, _) = migration_fault_fixture();
        let recovery_registry = recovery_plan.registry.clone();
        let initial = Arc::new(TracingIo::failing_at(initial_failure));
        assert!(transact_migration_with_io(&recovery_path, recovery_plan, initial).is_err());
        let recovery_recording = Arc::new(TracingIo::recording());
        recover_migration_with_io(
            &recovery_path,
            recovery_registry,
            recovery_recording.clone(),
        )
        .unwrap();
        let recovery_trace = recovery_recording.trace();

        for index in 0..recovery_trace.len() {
            let (_directory, path, plan, expected_new, expected_legacy_bytes) =
                migration_fault_fixture();
            let registry = plan.registry.clone();
            let initial = Arc::new(TracingIo::failing_at(initial_failure));
            assert!(transact_migration_with_io(&path, plan, initial).is_err());
            let recovery_failure = Arc::new(TracingIo::failing_at(index));
            let _ = recover_migration_with_io(&path, registry.clone(), recovery_failure);
            recover_migration_with_io(&path, registry.clone(), Arc::new(RealCatalogStoreIo))
                .unwrap();
            assert_known_migration_state_or_absent(
                &path,
                registry,
                &expected_legacy_bytes,
                &[expected_new],
            );
            assert_retained_journal_artifacts(&path);
        }
    }

    #[test]
    fn fresh_catalog_rejects_a_migration_marker() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        fs::write(paths.migration_marker, b"unexpected marker").unwrap();

        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_migration_incomplete");
    }

    #[test]
    fn journal_and_snapshot_byte_caps_fail_closed() {
        let (_directory, path) = projects_path();
        let store = ProjectCatalogStore::initialize_empty(path.clone()).unwrap();
        drop(store);
        let paths = ProjectCatalogPaths::derive(&path).unwrap();
        fs::write(&paths.journal, vec![b' '; MAX_JOURNAL_BYTES + 1]).unwrap();
        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_byte_limit");
    }

    #[test]
    fn oversized_valid_legacy_store_reports_migration_requirement() {
        let (_directory, path) = projects_path();
        let legacy = LegacyProjectStoreV1 {
            version: 1,
            projects: vec![LegacyProjectRecordV1 {
                project_id: "legacy-project".into(),
                repo_id: None,
                canonical_path: "x".repeat(MAX_PROJECT_CATALOG_BYTES + 1),
                registered_at: "2026-01-01T00:00:00Z".into(),
                is_git_repo: false,
                languages: BTreeSet::new(),
                aliases: BTreeSet::new(),
            }],
        };
        let bytes = serde_json::to_vec(&legacy).unwrap();
        assert!(bytes.len() > MAX_PROJECT_CATALOG_BYTES);
        assert!(bytes.len() < MAX_LEGACY_PROJECT_STORE_BYTES);
        fs::write(&path, bytes).unwrap();

        let error = ProjectCatalogStore::open_existing(path).unwrap_err();
        assert_eq!(
            error.code(),
            "error.project_catalog_legacy_store_requires_migration"
        );
        assert_eq!(
            ParticipantRoleV1::Catalog.max_bytes(),
            MAX_LEGACY_PROJECT_STORE_BYTES
        );
        assert_eq!(
            ImmutableAssetRoleV1::LegacyProjectStoreBackup.max_bytes(),
            MAX_LEGACY_PROJECT_STORE_BYTES
        );
    }

    #[test]
    fn test_fixture_starts_with_matching_contract_versions() {
        let catalog = CatalogSnapshotV2 {
            version: CATALOG_VERSION_V2,
            epoch: 1,
            origin: CatalogOriginV2::FreshV2 {},
            projects: BTreeMap::new(),
            repo_histories: BTreeMap::new(),
            ambiguous_namespaces: BTreeMap::new(),
            scope_migrations: BTreeMap::new(),
        };
        let attachments = AttachmentSnapshotV1 {
            version: ATTACHMENT_VERSION_V1,
            epoch: 1,
            attachments: BTreeMap::new(),
            scope_migration_proofs: BTreeMap::new(),
            legacy_path_bindings: BTreeMap::new(),
            default_attachments: BTreeMap::new(),
        };
        validate_catalog_attachments(&catalog, &attachments).unwrap();
    }
}

#[cfg(test)]
mod probe_tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentKind, AttachmentStatus, CheckoutAttachment,
        CorpusProject, ProjectId, ProjectScope,
    };

    fn projects_path(root: &Path) -> PathBuf {
        root.join("projects.json")
    }

    #[test]
    fn absent_store_with_no_siblings_is_bridge() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert_eq!(
            probe_project_store_mode(&projects_path(&root)).unwrap(),
            ProjectStoreProbe::AbsentBridge
        );
        // Lock files are excluded from the sibling probe.
        std::fs::write(root.join("projects.json.lock"), b"").unwrap();
        std::fs::write(root.join("project-catalog-migration.lock"), b"").unwrap();
        assert_eq!(
            probe_project_store_mode(&projects_path(&root)).unwrap(),
            ProjectStoreProbe::AbsentBridge
        );
    }

    #[test]
    fn absent_store_with_any_catalog_family_sibling_refuses() {
        for sibling in [
            "project-attachments.json",
            "project-catalog-transaction.json",
            "project-catalog-migration.json",
            "project-catalog-migration-receipt.json",
            "project-catalog-migration-assets",
            "project-catalog-stage",
            "project-catalog-backups",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            if sibling.ends_with(".json") {
                std::fs::write(root.join(sibling), b"{}").unwrap();
            } else {
                std::fs::create_dir(root.join(sibling)).unwrap();
            }
            let error = probe_project_store_mode(&projects_path(&root))
                .expect_err(&format!("sibling {sibling} must refuse"));
            assert_eq!(error.code(), "error.project_catalog_half_pair");
        }
    }

    #[test]
    fn version_probe_selects_bridge_and_catalog_modes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = projects_path(&root);

        std::fs::write(&path, br#"{"version":1,"projects":[]}"#).unwrap();
        assert_eq!(
            probe_project_store_mode(&path).unwrap(),
            ProjectStoreProbe::LegacyV1
        );
        std::fs::remove_file(&path).unwrap();

        let store = ProjectCatalogStore::initialize_empty(&path).unwrap();
        drop(store);
        assert_eq!(
            probe_project_store_mode(&path).unwrap(),
            ProjectStoreProbe::CatalogV2
        );

        // A healthy migrated store keeps its retained receipt and assets;
        // they must not block catalog mode when the catalog is present.
        std::fs::write(root.join("project-catalog-migration-receipt.json"), b"{}").unwrap();
        std::fs::create_dir(root.join("project-catalog-migration-assets")).unwrap();
        assert_eq!(
            probe_project_store_mode(&path).unwrap(),
            ProjectStoreProbe::CatalogV2
        );
    }

    #[test]
    fn unsupported_and_malformed_bytes_refuse() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = projects_path(&root);

        std::fs::write(&path, br#"{"version":3}"#).unwrap();
        let error = probe_project_store_mode(&path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_unsupported_version");

        std::fs::write(&path, b"not json").unwrap();
        let error = probe_project_store_mode(&path).unwrap_err();
        assert_eq!(error.code(), "error.project_catalog_invalid_snapshot");
    }

    // ---- R2F6: multi-attachment and default-only changed-project tests ----

    fn r2f6_helper_make_attachment(
        att_id: &str,
        project_id: &ProjectId,
        scope: &PublishedScope,
    ) -> CheckoutAttachment {
        let checkout_id = if att_id.starts_with("att_1") {
            "a".repeat(32)
        } else {
            "b".repeat(32)
        };
        CheckoutAttachment {
            attachment_id: AttachmentId::parse(att_id.to_string()).unwrap(),
            project_id: project_id.clone(),
            checkout_id,
            checkout_dir: format!("/tmp/{att_id}"),
            checkout_project_dir: format!("/tmp/{att_id}"),
            project_root_relpath: ".".into(),
            kind: AttachmentKind::Base,
            validated_scope: Some(scope.clone()),
            computed_repo_hint: None,
            branch_ref: None,
            capabilities: AttachmentCapabilities::default(),
            status: AttachmentStatus::Attached,
            attached_at: "2026-07-22T00:00:00Z".into(),
            detached_at: None,
        }
    }

    /// R2F6: when a project has two attachments and only the second one
    /// changes status, the project must appear in changed_project_ids.
    #[test]
    fn r2f6_two_attachments_second_changed_emits_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("projects.json");
        let store = ProjectCatalogStore::initialize_empty(path).unwrap();
        let observer = store.commit_observer();
        let pid = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
        let scope = PublishedScope::try_new("r2f6-repo", ".").unwrap();
        let att1 =
            r2f6_helper_make_attachment("att_11111111111111111111111111111111", &pid, &scope);
        let att2 =
            r2f6_helper_make_attachment("att_22222222222222222222222222222222", &pid, &scope);
        let pid_clone = pid.clone();
        let scope_clone = scope.clone();
        let att1_clone = att1.clone();
        let att2_clone = att2.clone();
        store
            .transact(1, move |catalog, attachments| {
                catalog.projects.insert(
                    pid_clone.clone(),
                    CorpusProject {
                        project_id: pid_clone.clone(),
                        scope: ProjectScope::Published(scope_clone.clone()),
                        operator_aliases: BTreeSet::new(),
                        nominated_aliases: BTreeSet::new(),
                        display_name: "r2f6".into(),
                        created_at: "2026-07-22T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: BTreeSet::new(),
                    },
                );
                attachments
                    .attachments
                    .insert(att1_clone.attachment_id.clone(), att1_clone);
                attachments
                    .attachments
                    .insert(att2_clone.attachment_id.clone(), att2_clone);
                Ok(())
            })
            .unwrap();

        // Clear the first commit's events.
        let _ = observer.drain_events();

        // Now change only att2 (detach it).
        store
            .transact(2, move |_, attachments| {
                let entry = attachments
                    .attachments
                    .get_mut(
                        &AttachmentId::parse("att_22222222222222222222222222222222".to_string())
                            .unwrap(),
                    )
                    .unwrap();
                entry.status = AttachmentStatus::Detached;
                entry.capabilities = AttachmentCapabilities::default();
                entry.detached_at = Some("2026-07-23T00:00:00Z".into());
                Ok(())
            })
            .unwrap();

        let events = observer.drain_events();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .changed_project_ids
                .contains("p_000000000000000000000000000000a1"),
            "project must be in changed_project_ids when second attachment changes, got: {:?}",
            events[0].changed_project_ids
        );
    }

    /// R2F6: a default_attachments-only change (no attachment content
    /// change) must emit the project in changed_project_ids.
    #[test]
    fn r2f6_default_attachment_only_change_emits_project() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("projects.json");
        let store = ProjectCatalogStore::initialize_empty(path).unwrap();
        let observer = store.commit_observer();
        let pid = ProjectId::parse("p_000000000000000000000000000000a1").unwrap();
        let scope = PublishedScope::try_new("r2f6-repo2", ".").unwrap();
        let att1 =
            r2f6_helper_make_attachment("att_11111111111111111111111111111111", &pid, &scope);
        let att2 =
            r2f6_helper_make_attachment("att_22222222222222222222222222222222", &pid, &scope);
        let pid_clone = pid.clone();
        let pid_clone2 = pid.clone();
        let scope_clone = scope.clone();
        let att1_clone = att1.clone();
        let att2_clone = att2.clone();
        store
            .transact(1, move |catalog, attachments| {
                catalog.projects.insert(
                    pid_clone.clone(),
                    CorpusProject {
                        project_id: pid_clone.clone(),
                        scope: ProjectScope::Published(scope_clone.clone()),
                        operator_aliases: BTreeSet::new(),
                        nominated_aliases: BTreeSet::new(),
                        display_name: "r2f6".into(),
                        created_at: "2026-07-22T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: BTreeSet::new(),
                    },
                );
                attachments
                    .attachments
                    .insert(att1_clone.attachment_id.clone(), att1_clone);
                attachments
                    .attachments
                    .insert(att2_clone.attachment_id.clone(), att2_clone);
                // Set default to att1.
                attachments.default_attachments.insert(
                    pid_clone.clone(),
                    AttachmentId::parse("att_11111111111111111111111111111111".to_string())
                        .unwrap(),
                );
                Ok(())
            })
            .unwrap();

        // Clear the first commit's events.
        let _ = observer.drain_events();

        // Now change only the default attachment to att2 (no attachment
        // content change).
        let pid_clone3 = pid_clone2.clone();
        store
            .transact(2, move |_, attachments| {
                attachments.default_attachments.insert(
                    pid_clone3.clone(),
                    AttachmentId::parse("att_22222222222222222222222222222222".to_string())
                        .unwrap(),
                );
                Ok(())
            })
            .unwrap();

        let events = observer.drain_events();
        assert_eq!(events.len(), 1);
        assert!(
            events[0]
                .changed_project_ids
                .contains("p_000000000000000000000000000000a1"),
            "project must be in changed_project_ids when default attachment changes, got: {:?}",
            events[0].changed_project_ids
        );
    }
}

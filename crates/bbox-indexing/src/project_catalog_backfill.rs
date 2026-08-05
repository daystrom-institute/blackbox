//! Closed public authority boundary for the Phase 6 durable backfill.
//!
//! `durable-backfill` owns the governing section 7.3 path-keyed durable-store
//! row stamping (Phase 6 plan section 3.3). It iterates the legacy path ledger
//! installed by the migration transaction, classifies every effective binding
//! as mappable, quarantined, or unscoped, stamps the mappable rows' owning
//! durable-store rows with the stable `ProjectId`, and records the result in a
//! standalone completion journal that lives OUTSIDE the catalog pair.
//!
//! Three properties are load-bearing and are why this module exists rather
//! than a helper inside the migration facade:
//!
//! 1. **The pair usually does not move.** Stamping writes durable stores, not
//!    the catalog pair, so the post-image catalog epoch equals the predecessor
//!    epoch unless the resolution converts quarantined bindings. The rebuild
//!    preflight still needs a contiguous four-hash chain, which is exactly what
//!    [`BackfillCompletionJournalV1`] provides.
//! 2. **Quarantine rides forward.** An unresolved quarantined binding is
//!    COUNTED, never a blocker. It gates the later path-fallback removal, not
//!    the Phase 6 cut.
//! 3. **Publisher evidence is verified, never seeded.** Publisher bindings and
//!    G1 content are installed by the migration transaction (governing 13.2,
//!    D-006, D-014). This command proves them per disposition and refuses a
//!    defect; first publication remains the D-040 Establish operation.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bbox_corpus_core::json_store::NofollowDirectory;
use bbox_corpus_core::project_catalog::{
    AttachmentSnapshotV1, LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry,
    LegacyPathRelationship, ProjectId,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_catalog_inventory::{
    LegacyPathStoreKindV1, OperatorResolutionNoteV1, Sha256ValueV1, digest_path,
    digest_published_scope, digest_publisher_full_ref,
};
use crate::project_catalog_migration::{
    ProjectCatalogMigrationError, ProjectCatalogMigrationMutationDispositionV1,
    ProjectCatalogMigrationResolvedLayoutV1, ProjectCatalogTargetSelectionV1,
    validate_artifact_set, validate_target_selection,
};
use crate::project_catalog_store::{ProjectCatalogStore, PublisherDispositionEvidenceV1};
use bbox_corpus_core::identity::PublishedScope;

pub const DURABLE_BACKFILL_REPORT_VERSION_V1: u32 = 1;
pub const DURABLE_BACKFILL_RESOLUTION_VERSION_V1: u32 = 1;
pub const BACKFILL_COMPLETION_JOURNAL_VERSION_V1: u32 = 1;

/// The completion journal's fixed filename beside the store (section 3.3).
pub const BACKFILL_COMPLETION_JOURNAL_FILE: &str = "backfill-completion.json";

const MAX_BACKFILL_ARTIFACT_BYTES: usize = 8 * 1024 * 1024;
const MAX_BACKFILL_JOURNAL_BYTES: usize = 4 * 1024 * 1024;

/// The plan's NEW resolution refusal code (section 7.3). A resolution naming a
/// disposition inconsistent with the predecessor inventory refuses with it.
///
/// Public, with [`ERROR_STALE_POST_IMAGE`], because the production row stamper
/// lives in the root crate (it needs the real owner schemas) and must choose
/// between these two codes per adjudication Q-E4. Exporting the constants keeps
/// ONE code vocabulary; a root-side string literal would be a second one that
/// drifts silently, which section 7 forbids.
pub const ERROR_RESOLUTION_INVALID: &str =
    "error.project_catalog_durable_backfill_resolution_invalid";
/// The shipped four-hash identity refusal (section 7.1). Reused verbatim
/// rather than minting a backfill-specific twin: section 7.3 admits new codes
/// only for the four the plan names.
const ERROR_ARTIFACT_IDENTITY: &str = "error.project_catalog_migration_artifact_identity";
/// The shipped staleness suffix family (section 7.2).
const ERROR_STALE_REPORT: &str = "error.project_catalog_inventory_stale_report";
const ERROR_STALE_RESOLUTION: &str = "error.project_catalog_inventory_stale_resolution";
/// Apply-time divergence between the preflight post-image and what the owner
/// actually holds (adjudication Q-E4). Public for the root-crate stamper; see
/// [`ERROR_RESOLUTION_INVALID`].
pub const ERROR_STALE_POST_IMAGE: &str = "error.project_catalog_inventory_stale_post_image";

type BackfillResult<T> = Result<T, ProjectCatalogMigrationError>;

fn refuse(code: &'static str, message: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(code, message)
}

/// A refusal raised AFTER apply began mutating durable state.
///
/// Never `NoDurableMutation`: past the pair transaction, an appended
/// supersession has committed and an unknown prefix of the stamp set has landed,
/// so the only truthful disposition is the one that sends the operator to a
/// fresh preflight and re-apply. This is the same rule
/// `with_backfill_stamping_disposition` applies to stamping refusals, stated for
/// the failures raised by the facade itself rather than by the stamper.
fn post_mutation_refusal(
    code: &'static str,
    message: impl Into<String>,
) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::new(
        code,
        message,
        ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired,
    )
}

// ---------------------------------------------------------------------------
// Ledger supersession
// ---------------------------------------------------------------------------

/// The append-only ledger's identity key: `(historical_path, source_store,
/// source_row_id)`. `LegacyPathBindingId`s are unique randoms and
/// `validate_catalog` imposes no uniqueness constraint on this triple, so the
/// plan (section 3.3) defines supersession over it explicitly.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct LegacyLedgerKeyV1 {
    pub historical_path: String,
    pub source_store: String,
    pub source_row_id: String,
}

impl LegacyLedgerKeyV1 {
    fn of(entry: &LegacyPathLedgerEntry) -> Self {
        Self {
            historical_path: entry.historical_path.clone(),
            source_store: entry.source_store.clone(),
            source_row_id: entry.source_row_id.clone(),
        }
    }
}

/// One key's effective binding plus the audit trail it superseded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveLegacyBindingV1 {
    pub effective: LegacyPathLedgerEntry,
    /// Bindings retained as audit records under the same key. Never dropped:
    /// the plan keeps the quarantined original after a resolution converts it.
    pub superseded: Vec<LegacyPathLedgerEntry>,
}

/// Resolve the append-only ledger to one effective binding per key.
///
/// The rule, exactly as section 3.3 states it: for equal
/// `(historical_path, source_store, source_row_id)`, the binding with the
/// highest `inventory_epoch` supersedes. This single function drives BOTH the
/// backfill's classification counts and the later fallback-removal gate's
/// quarantine-emptiness check, so the two can never disagree.
///
/// An epoch TIE is not silently broken by iteration order. Two bindings at the
/// same epoch under one key can only come from a single inventory pass, which
/// never emits duplicates; if they nonetheless disagree on status the ledger is
/// inconsistent and this refuses rather than picking a winner. A tie whose
/// statuses agree is harmless and resolves deterministically by binding id.
pub fn effective_legacy_bindings(
    bindings: &BTreeMap<LegacyPathBindingId, LegacyPathLedgerEntry>,
) -> BackfillResult<BTreeMap<LegacyLedgerKeyV1, EffectiveLegacyBindingV1>> {
    let mut grouped: BTreeMap<LegacyLedgerKeyV1, Vec<LegacyPathLedgerEntry>> = BTreeMap::new();
    for entry in bindings.values() {
        grouped
            .entry(LegacyLedgerKeyV1::of(entry))
            .or_default()
            .push(entry.clone());
    }
    let mut effective = BTreeMap::new();
    for (key, mut entries) in grouped {
        // Highest epoch first; then binding id, so the tie case is total.
        entries.sort_by(|left, right| {
            right
                .inventory_epoch
                .cmp(&left.inventory_epoch)
                .then_with(|| {
                    right
                        .legacy_path_binding_id
                        .as_str()
                        .cmp(left.legacy_path_binding_id.as_str())
                })
        });
        let top_epoch = entries[0].inventory_epoch;
        let tied = entries
            .iter()
            .filter(|entry| entry.inventory_epoch == top_epoch)
            .collect::<Vec<_>>();
        if tied.len() > 1 && tied.iter().any(|entry| entry.status != tied[0].status) {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "legacy path ledger holds disagreeing bindings at one key and epoch",
            ));
        }
        let winner = entries.remove(0);
        effective.insert(
            key,
            EffectiveLegacyBindingV1 {
                effective: winner,
                superseded: entries,
            },
        );
    }
    Ok(effective)
}

/// The classification the backfill acts on, derived from the EFFECTIVE status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyRowClassificationV1 {
    /// `Mapped { .. }`: the row's `project_id` is stamped, idempotently.
    Mappable,
    /// `Quarantined {}`: counted, never stamped, rides forward.
    Quarantined,
    /// `Unscoped {}`: counted by store, never stamped. These rows never
    /// belonged to an inventory-time registered root.
    Unscoped,
}

impl LegacyRowClassificationV1 {
    fn of(status: &LegacyPathBindingStatus) -> Self {
        match status {
            LegacyPathBindingStatus::Mapped { .. } => Self::Mappable,
            LegacyPathBindingStatus::Quarantined {} => Self::Quarantined,
            LegacyPathBindingStatus::Unscoped {} => Self::Unscoped,
        }
    }
}

/// Map a ledger `source_store` token back to its owner variant.
///
/// The token vocabulary is the migration's; this is its inverse and must stay
/// exhaustive over the 14-variant owner set. An unknown token is a ledger the
/// backfill cannot act on, not a row to skip silently.
pub fn legacy_store_kind_from_token(token: &str) -> Option<LegacyPathStoreKindV1> {
    Some(match token {
        "knowledge" => LegacyPathStoreKindV1::Knowledge,
        "gap" => LegacyPathStoreKindV1::Gap,
        "thread" => LegacyPathStoreKindV1::Thread,
        "note" => LegacyPathStoreKindV1::Note,
        "pin" => LegacyPathStoreKindV1::Pin,
        "roadmap" => LegacyPathStoreKindV1::Roadmap,
        "packet" => LegacyPathStoreKindV1::Packet,
        "task" => LegacyPathStoreKindV1::Task,
        "proposal" => LegacyPathStoreKindV1::Proposal,
        "slack" => LegacyPathStoreKindV1::SlackBinding,
        "whiteboard" => LegacyPathStoreKindV1::Whiteboard,
        "artifact" => LegacyPathStoreKindV1::Artifact,
        "provenance" => LegacyPathStoreKindV1::Provenance,
        "transcript-edge" => LegacyPathStoreKindV1::TranscriptEdge,
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// One classified ledger row, path-redacted.
///
/// `historical_path` is a host-local literal and never enters a persisted
/// artifact; only its digest does, exactly as the migration report's
/// `path_digest` rows do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillLedgerRowV1 {
    pub legacy_path_binding_id: LegacyPathBindingId,
    pub store_kind: LegacyPathStoreKindV1,
    pub historical_path_digest: Sha256ValueV1,
    pub source_row_id: String,
    pub inventory_epoch: u64,
    pub classification: LegacyRowClassificationV1,
    /// Present exactly when the classification is `Mappable`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<ProjectId>,
    /// How many bindings this row supersedes under the section 3.3 rule.
    pub superseded_count: u64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyStoreClassificationCountsV1 {
    pub mappable: u64,
    pub quarantined: u64,
    pub unscoped: u64,
    pub superseded: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherDispositionKindV1 {
    SeedG1,
    NoPublishedContentAcknowledged,
}

/// What the per-disposition publisher/G1 proof observed (D-040, section 3.3).
///
/// The two branches prove OPPOSITE things and neither seeds. `SeedG1` proves a
/// pointer and its G1 generation are present and hash-consistent;
/// `NoPublishedContentAcknowledged` proves the pointer is ABSENT, because
/// D-040 reserves first publication for an explicit Establish. A pointer found
/// on that branch is a defect refusal, not a gap to fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublisherVerificationOutcomeV1 {
    PointerPresentAndConsistent,
    PointerMissing,
    PointerHashMismatch,
    GenerationMissing,
    PointerAbsentAsRequired,
    PointerUnexpectedlyPresent,
}

impl PublisherVerificationOutcomeV1 {
    pub fn is_defect(self) -> bool {
        !matches!(
            self,
            Self::PointerPresentAndConsistent | Self::PointerAbsentAsRequired
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublisherDispositionVerificationV1 {
    pub project_id: ProjectId,
    pub kind: PublisherDispositionKindV1,
    pub expected_scope_digest: Sha256ValueV1,
    pub full_ref_digest: Sha256ValueV1,
    pub outcome: PublisherVerificationOutcomeV1,
}

/// Backfill status. There is deliberately no `ResolutionRequired`: section 3.3
/// makes unresolved quarantine ride forward as counted state, so quarantine
/// alone never gates the cut. Only a publisher/G1 defect refuses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableBackfillStatusV1 {
    Clean,
    Refused,
}

/// The reviewed backfill report (FD-3 artifact vocabulary, FD-4 acyclicity).
///
/// It carries the inventory hash, the plan hash, and the resolution artifact
/// hash. It never carries its own byte hash, and the resolution never carries
/// the report's, so the artifact hash graph stays acyclic; the four-hash
/// identity lives in the completion journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableBackfillReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub status: DurableBackfillStatusV1,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub predecessor_catalog_epoch: u64,
    pub predecessor_catalog_hash: Sha256ValueV1,
    pub predecessor_attachment_hash: Sha256ValueV1,
    pub ledger_rows: Vec<BackfillLedgerRowV1>,
    pub classification_counts: BTreeMap<LegacyPathStoreKindV1, LegacyStoreClassificationCountsV1>,
    /// Planned idempotent stamp operations per store.
    pub planned_stamps: BTreeMap<LegacyPathStoreKindV1, u64>,
    /// Rows that never belonged to a registered root: counted, never stamped.
    pub unscoped_legacy_counts: BTreeMap<LegacyPathStoreKindV1, u64>,
    pub publisher_verification: Vec<PublisherDispositionVerificationV1>,
    /// Per-store stamper coverage for every store with planned stamps. A
    /// `NotImplemented` row refuses the report rather than being omitted, so
    /// an unstampable owner can never be mistaken for an empty one.
    pub stamp_coverage: BTreeMap<LegacyPathStoreKindV1, LegacyRowStampCoverageV1>,
    /// Equals `predecessor_catalog_epoch` when the resolution converts nothing:
    /// mappable-row stamping writes durable stores and does NOT bump the pair.
    pub predicted_post_image_catalog_epoch: u64,
    /// Where apply writes the journal the rebuild preflight reads as its
    /// predecessor binding. Relative to the state directory, never absolute:
    /// a persisted artifact carries no host path.
    pub completion_journal_relative_path: String,
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// The three dispositions section 3.3 admits for a quarantined binding.
///
/// There is no delete disposition and no `Deleted` ledger status to express
/// one: `LegacyPathBindingStatus` has exactly three variants, inventing a
/// tombstone would be unmandated new substrate, and deleting legacy rows during
/// the cut is destructive discretion Phase 6 refuses. The absence is enforced
/// by the type plus `deny_unknown_fields`, so an artifact naming
/// `acknowledged-delete` fails to decode and refuses with
/// `error.project_catalog_durable_backfill_resolution_invalid` rather than
/// being silently ignored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "disposition", rename_all = "snake_case", deny_unknown_fields)]
pub enum LegacyPathDispositionV1 {
    /// Append a superseding `Mapped` binding at a higher inventory epoch.
    MapToProjectId {
        legacy_path_binding_id: LegacyPathBindingId,
        project_id: ProjectId,
        relationship: LegacyPathRelationship,
    },
    /// Leave the binding in counted quarantine. Appends nothing.
    RetainQuarantine {
        legacy_path_binding_id: LegacyPathBindingId,
    },
    /// Append a superseding `Unscoped {}` binding at a higher inventory epoch.
    ClassifyUnscopedByAppendedSupersession {
        legacy_path_binding_id: LegacyPathBindingId,
    },
}

impl LegacyPathDispositionV1 {
    pub fn legacy_path_binding_id(&self) -> &LegacyPathBindingId {
        match self {
            Self::MapToProjectId {
                legacy_path_binding_id,
                ..
            }
            | Self::RetainQuarantine {
                legacy_path_binding_id,
            }
            | Self::ClassifyUnscopedByAppendedSupersession {
                legacy_path_binding_id,
            } => legacy_path_binding_id,
        }
    }

    /// Whether this disposition appends a superseding binding, i.e. whether it
    /// makes the pair transaction run and the catalog epoch move.
    fn appends(&self) -> bool {
        !matches!(self, Self::RetainQuarantine { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DurableBackfillResolutionV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    pub legacy_path_dispositions: Vec<LegacyPathDispositionV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_notes: Vec<OperatorResolutionNoteV1>,
}

impl DurableBackfillResolutionV1 {
    /// The canonical empty resolution a deterministic operation writes when it
    /// has nothing to resolve (FD-3).
    pub fn empty(inventory_hash: Sha256ValueV1) -> Self {
        Self {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: Vec::new(),
            operator_notes: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Completion journal
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillStoreStampCountsV1 {
    pub mappable: u64,
    pub stamped: u64,
    /// Rows that were already stamped. Stamping is idempotent, so a partially
    /// stamped set from a torn apply completes without duplication.
    pub already_stamped: u64,
    pub converted: u64,
    pub unscoped: u64,
}

/// The four-hash identity the whole cut chain is bound by (FD-3, FD-4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillArtifactIdentityV1 {
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
}

/// The standalone backfill completion record (section 3.3).
///
/// It lives OUTSIDE the catalog pair at `<state>/backfill-completion.json`,
/// following the `ProjectRetirementJournal` offline-journal precedent (Phase 4
/// section 11), and is fsynced before apply returns. It exists because the pair
/// usually does not move: mappable-row stamping writes durable stores without
/// bumping the catalog epoch, so without this file the rebuild preflight would
/// have no predecessor binding distinguishing "backfilled" from "not yet".
///
/// It is origin-scoped to `MigratedV1` by construction: a `FreshV2` store never
/// runs a backfill and never produces one (section 10.1, D-011).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackfillCompletionJournalV1 {
    pub version: u32,
    pub completed_at: String,
    /// The epoch and snapshot hash the applied plan was validated against.
    pub predecessor_catalog_epoch: u64,
    pub predecessor_catalog_hash: Sha256ValueV1,
    pub predecessor_attachment_hash: Sha256ValueV1,
    /// Equals the predecessor epoch when no quarantine conversion landed.
    pub post_image_catalog_epoch: u64,
    pub stamp_counts: BTreeMap<LegacyPathStoreKindV1, BackfillStoreStampCountsV1>,
    /// The EXACT bindings this apply appended, naming its conversions
    /// individually rather than only counting them per store.
    ///
    /// Counts alone cannot be verified per store. Verify can bound a store's
    /// conversions by the rows that could carry one, but a bound admits any
    /// redistribution that preserves the total, so a journal could move
    /// conversions between owners and still satisfy every count and the epoch
    /// transition. Naming the bindings makes the population exact and
    /// checkable: verify resolves each id in the durable ledger, derives its
    /// store from the LEDGER's own `source_store` rather than from the
    /// journal's grouping, and requires the per-store totals to match. It also
    /// scopes the claim to THIS apply's wave, which a ledger-derived count
    /// cannot do once a second backfill has run over the same target.
    pub converted_binding_ids: BTreeSet<LegacyPathBindingId>,
    pub identity: BackfillArtifactIdentityV1,
    /// The publisher/G1 disposition set apply committed against, digested at its
    /// FINAL publication boundary.
    ///
    /// Accepted-publication state sits outside the four-hash identity, so this
    /// is the only durable record of what the applied backfill actually
    /// observed. Verify re-proves the dispositions and compares this digest, so
    /// "the publication state moved since apply" is a distinct, checkable
    /// answer rather than an unnoticed one.
    pub publisher_verification_digest: Sha256ValueV1,
}

impl BackfillCompletionJournalV1 {
    /// Total stamped-or-already-stamped rows, the set backfill verify proves
    /// against the ledger.
    pub fn applied_stamp_total(&self) -> u64 {
        self.stamp_counts
            .values()
            .map(|counts| counts.stamped + counts.already_stamped)
            .sum()
    }
}

/// The journal path for a state directory.
pub fn backfill_completion_journal_path(state_dir: &Path) -> PathBuf {
    state_dir.join(BACKFILL_COMPLETION_JOURNAL_FILE)
}

/// Read the completion journal, `None` when absent.
///
/// Absence after a crash is not an error: section 3.3's torn-backfill recovery
/// is a fresh preflight and re-apply, whose predecessor epoch and four-hash
/// identity are re-validated from scratch.
pub fn read_backfill_completion_journal(
    state_dir: &Path,
) -> BackfillResult<Option<BackfillCompletionJournalV1>> {
    let path = backfill_completion_journal_path(state_dir);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                format!("backfill completion journal cannot be read: {error}"),
            ));
        }
    };
    if bytes.len() > MAX_BACKFILL_JOURNAL_BYTES {
        return Err(refuse(
            ERROR_STALE_POST_IMAGE,
            "backfill completion journal exceeds its byte bound",
        ));
    }
    let journal: BackfillCompletionJournalV1 = serde_json::from_slice(&bytes).map_err(|error| {
        refuse(
            ERROR_STALE_POST_IMAGE,
            format!("backfill completion journal cannot be decoded: {error}"),
        )
    })?;
    if journal.version != BACKFILL_COMPLETION_JOURNAL_VERSION_V1 {
        return Err(refuse(
            ERROR_STALE_POST_IMAGE,
            "backfill completion journal version is not supported",
        ));
    }
    Ok(Some(journal))
}

/// The two points a journal publication can fail at, in order.
///
/// Named rather than implicit because the two sides of the rename are
/// OBSERVABLY different failures and both must be handled: before it, nothing
/// is published; after it, the bytes are visible but not yet proven durable.
/// The production path passes a no-op checkpoint; the tests below drive a fault
/// at each so both sides are covered by a real refusal rather than by argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JournalDurabilityStepV1 {
    BeforeReplace,
    /// After the atomic replacement landed, immediately before the parent
    /// directory fsync that makes the new directory entry durable.
    AfterReplace,
}

/// Publish the completion journal durably beside the store, or refuse.
///
/// Three properties are load-bearing, and each was missing:
///
/// 1. **Unique and confined.** The publication rides a NOFOLLOW handle on the
///    state directory, and the temporary is an `O_EXCL` name unique per process
///    and per call. A FIXED staging name is not merely untidy: a leftover from a
///    torn or concurrent publication wedges every later one when it is a
///    directory, and REDIRECTS the write outside the state directory when it is
///    a symlink.
/// 2. **The parent fsync is mandatory.** A rename is durable only once the
///    directory entry is, and this record is exactly what the rebuild preflight
///    later reads as proof the stamp set landed. A directory that refuses an
///    fsync handle therefore FAILS the publication; the previous revision opened
///    it best-effort and discarded both the open error and the fsync error.
/// 3. **Every failure here is post-mutation.** By the time this runs, any
///    appended supersession has committed to the pair and the stamp set has
///    landed, so a refusal is `RetryExactPlanRequired`. Reporting
///    `NoDurableMutation` would tell the operator nothing happened while the
///    owner stores have already moved.
fn write_backfill_completion_journal(
    state_dir: &Path,
    journal: &BackfillCompletionJournalV1,
) -> BackfillResult<()> {
    publish_backfill_completion_journal(state_dir, journal, |_step| Ok(()))
}

fn publish_backfill_completion_journal(
    state_dir: &Path,
    journal: &BackfillCompletionJournalV1,
    checkpoint: impl Fn(JournalDurabilityStepV1) -> BackfillResult<()>,
) -> BackfillResult<()> {
    let bytes = serde_json::to_vec(journal).map_err(|error| {
        post_mutation_refusal(
            ERROR_STALE_POST_IMAGE,
            format!("backfill completion journal cannot be encoded: {error}"),
        )
    })?;
    if bytes.is_empty() || bytes.len() > MAX_BACKFILL_JOURNAL_BYTES {
        return Err(post_mutation_refusal(
            ERROR_STALE_POST_IMAGE,
            "backfill completion journal bytes are empty or exceed their bound",
        ));
    }
    // Fixed diagnostics rather than the underlying error: these carry directory
    // paths, and a facade refusal is path-redacted.
    let io_refusal = |detail: &'static str| {
        post_mutation_refusal(
            ERROR_STALE_POST_IMAGE,
            format!("backfill completion journal cannot be published: {detail}"),
        )
    };
    let directory = NofollowDirectory::open_existing(state_dir)
        .map_err(|_| io_refusal("the state directory is unsafe or unreadable"))?
        .ok_or_else(|| io_refusal("the state directory is absent"))?;
    checkpoint(JournalDurabilityStepV1::BeforeReplace)?;
    directory
        .atomic_replace(BACKFILL_COMPLETION_JOURNAL_FILE, &bytes)
        .map_err(|_| io_refusal("the atomic replacement failed"))?;
    checkpoint(JournalDurabilityStepV1::AfterReplace)?;
    // The replacement fsyncs the parent itself, and this call site syncs it
    // AGAIN rather than relying on that: the contract "durable when this
    // returns" belongs to this function, not to a helper's internal choice, and
    // an fsync of an already-synced directory costs nothing.
    directory
        .sync_all()
        .map_err(|_| io_refusal("the parent directory fsync failed"))?;
    directory
        .ensure_still_current()
        .map_err(|_| io_refusal("the state directory changed during publication"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Stamping seam
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyRowStampOutcomeV1 {
    /// The row carried no stable id and now carries this one.
    Stamped,
    /// The row already carried exactly this id. Stamping is idempotent, so a
    /// re-apply after a torn stamping pass completes without duplication.
    AlreadyStamped,
}

/// The durable-store write seam the backfill stamps through.
///
/// It is an injected dependency rather than a hard-wired call for three
/// reasons. First, the 14 `LegacyPathStoreKindV1` owners live in their own
/// crates and expose only READ captures today, so the write half must be able
/// to land independently of this facade. Second, it keeps the facade's
/// transaction boundary testable without 14 real stores. Third, it is the
/// injection point P6-C's torn-backfill fault needs: a stamper that fails
/// partway through the set exercises exactly the recovery section 3.3
/// specifies, with no production code aware of the fault.
///
/// Implementations MUST be idempotent per `(store_kind, source_row_id)`:
/// stamping a row that already carries the same `ProjectId` returns
/// `AlreadyStamped` and writes nothing.
pub trait LegacyRowStamperV1: Send + Sync {
    /// Whether this stamper can write the owner behind `store_kind`.
    ///
    /// Deliberately a probe rather than a registry lookup: an implementation
    /// answers with an exhaustive `match` over the 14-variant owner set, so
    /// the compiler forces it to answer for every store and polices any
    /// variant added later. A registry of optional per-store stampers would
    /// let three-of-fourteen coverage pass as silent absence, which is the
    /// exact failure this shape exists to make impossible.
    ///
    /// Preflight probes every store with planned stamps and records the answer
    /// per store in the report, refusing the whole report when any planned
    /// store is `NotImplemented`. An owner whose write-back has not landed is
    /// therefore a TYPED, per-store, pre-mutation refusal, never a zero that
    /// reads like "nothing to do".
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1;

    fn stamp(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_id: &str,
        project_id: &ProjectId,
    ) -> BackfillResult<LegacyRowStampOutcomeV1>;
}

/// What a read-side probe observed about one durable owner row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyRowObservationV1 {
    /// The row exists and carries exactly this stable project id.
    StampedWith(ProjectId),
    /// The row exists and carries no stable project id.
    Unstamped,
    /// The owner holds no such row.
    Absent,
}

/// The durable-store READ seam the backfill verifies through.
///
/// Symmetric to [`LegacyRowStamperV1`] and separate from it on purpose. Verify
/// must never be able to write: an owner verifier that could stamp would be
/// proving its own work. It also cannot be answered from the owners' capture
/// lane, which reports which rows still carry a legacy selector and says
/// nothing about the `project_id` a row now holds - the very field the backfill
/// wrote.
///
/// Without this, verify compared the completion journal's stamp counts against
/// the catalog ledger's mappable counts and never opened a single owner store,
/// so a stamper that returned `Stamped` without writing anything produced a
/// journal that verified perfectly against rows that had never been touched.
///
/// Implementations answer `coverage` by exhaustive `match` over the 14-variant
/// owner set, for the reason spelled out on [`LegacyRowStamperV1::coverage`].
pub trait LegacyRowOwnerReaderV1: Send + Sync {
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1;

    /// Observe every named row of ONE owner.
    ///
    /// Batched per owner rather than per row because several owners are trees
    /// or lane sets whose row lookup is a full walk; a per-row call would make
    /// verifying an owner quadratic in its rows.
    ///
    /// Implementations MUST honour that batching rather than merely accept it:
    /// ONE lock-and-decode, or ONE tree walk, per owner per call. It is a
    /// correctness obligation and not only a cost one. An implementation that
    /// looped a per-row read here would capture the owner once per row, so the
    /// answers it returned for one owner could come from several different
    /// durable states of that store and describe a combination that never
    /// existed - while also turning verify into O(rows x owner size), which is
    /// spent inside a stopped-service closeout window. The production reader is
    /// held to this by
    /// `bbox_corpus_core::project_catalog_snapshot::owner_row_read_captures`,
    /// which counts the captures a batch actually performs.
    fn observe(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_ids: &BTreeSet<String>,
    ) -> BackfillResult<BTreeMap<String, LegacyRowObservationV1>>;
}

/// Whether an owner's durable-store write-back is available to the backfill.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LegacyRowStampCoverageV1 {
    Covered,
    /// The owner crate exposes no write-back yet. Counted and named per store
    /// rather than skipped.
    NotImplemented,
    /// The owner cannot produce a stamping obligation at all, so there is
    /// nothing to write and nothing missing (adjudication Q-E3b).
    ///
    /// Distinct from `Covered` with a no-op stamper, and the distinction is the
    /// point. `Covered` asserts "I can write this owner", which invites a
    /// silent zero when a real obligation appears; this asserts the stronger
    /// and checkable "a legitimate predecessor cannot ASK me to write this
    /// owner", which preflight then enforces by refusing any positive planned
    /// count and any effective ledger binding naming the owner. An exemption
    /// that stopped being true would therefore fail loudly instead of stamping
    /// nothing and reporting success.
    ExemptByConstruction,
}

/// The planned stores this stamper cannot write, in owner-token form.
///
/// Shared by preflight (which turns a nonempty result into a refused report)
/// and apply (which turns it into a pre-mutation refusal), so the two can never
/// disagree about what "covered" means.
pub fn uncovered_planned_stores(
    planned_stamps: &BTreeMap<LegacyPathStoreKindV1, u64>,
    stamper: &dyn LegacyRowStamperV1,
) -> Vec<&'static str> {
    planned_stamps
        .iter()
        // A store with zero planned stamps needs no write-back: refusing on it
        // would block a backfill that was never going to touch that owner.
        .filter(|(_, planned)| **planned > 0)
        .filter(|(kind, _)| stamper.coverage(**kind) == LegacyRowStampCoverageV1::NotImplemented)
        .map(|(kind, _)| legacy_store_token(*kind))
        .collect()
}

/// Planned stores whose owner is exempt by construction, in owner-token form.
///
/// A NONEMPTY result means the exemption's premise has failed: something
/// produced a stamping obligation for an owner that, by the argument in
/// [`LegacyRowStampCoverageV1::ExemptByConstruction`], cannot have one. That is
/// an inconsistent predecessor, not work to do, so preflight and apply both
/// refuse on it before any mutation rather than stamping it or quietly
/// counting zero (adjudication Q-E3b).
pub fn exempt_stores_with_planned_stamps(
    planned_stamps: &BTreeMap<LegacyPathStoreKindV1, u64>,
    stamper: &dyn LegacyRowStamperV1,
) -> Vec<&'static str> {
    planned_stamps
        .iter()
        .filter(|(_, planned)| **planned > 0)
        .filter(|(kind, _)| {
            stamper.coverage(**kind) == LegacyRowStampCoverageV1::ExemptByConstruction
        })
        .map(|(kind, _)| legacy_store_token(*kind))
        .collect()
}

// ---------------------------------------------------------------------------
// Hashing
// ---------------------------------------------------------------------------

fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

/// Canonical hash over the predecessor the plan was captured from.
///
/// The inventory is the EFFECTIVE ledger plus the predecessor epoch and pair
/// hashes: precisely the inputs a plan is a function of. A report captured
/// against one root therefore cannot authorize an apply on another, which is
/// the cross-root guard section 3.1 relies on.
fn backfill_inventory_hash(
    epoch: u64,
    catalog_hash: &Sha256ValueV1,
    attachment_hash: &Sha256ValueV1,
    effective: &BTreeMap<LegacyLedgerKeyV1, EffectiveLegacyBindingV1>,
) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    field(
        &mut hasher,
        b"blackbox.project-catalog.backfill-inventory.v1",
    );
    hasher.update(epoch.to_be_bytes());
    field(&mut hasher, catalog_hash.as_str().as_bytes());
    field(&mut hasher, attachment_hash.as_str().as_bytes());
    for (key, binding) in effective {
        field(&mut hasher, key.historical_path.as_bytes());
        field(&mut hasher, key.source_store.as_bytes());
        field(&mut hasher, key.source_row_id.as_bytes());
        hasher.update(binding.effective.inventory_epoch.to_be_bytes());
        field(
            &mut hasher,
            binding.effective.legacy_path_binding_id.as_str().as_bytes(),
        );
        match &binding.effective.status {
            LegacyPathBindingStatus::Mapped {
                project_id,
                relationship,
            } => {
                field(&mut hasher, b"mapped");
                field(&mut hasher, project_id.as_str().as_bytes());
                field(
                    &mut hasher,
                    match relationship {
                        LegacyPathRelationship::Root => b"root".as_slice(),
                        LegacyPathRelationship::ContainedSubdirectory => b"contained".as_slice(),
                    },
                );
            }
            LegacyPathBindingStatus::Unscoped {} => field(&mut hasher, b"unscoped"),
            LegacyPathBindingStatus::Quarantined {} => field(&mut hasher, b"quarantined"),
        }
    }
    Sha256ValueV1::parse(hex::encode(hasher.finalize())).expect("code-owned digest is valid")
}

/// Canonical hash over the executable plan: the exact stamp set and the exact
/// appended supersessions, in a deterministic order.
fn backfill_plan_hash(
    inventory_hash: &Sha256ValueV1,
    stamps: &[(LegacyPathStoreKindV1, String, ProjectId)],
    appended: &[LegacyPathLedgerEntry],
) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    field(&mut hasher, b"blackbox.project-catalog.backfill-plan.v1");
    field(&mut hasher, inventory_hash.as_str().as_bytes());
    hasher.update((stamps.len() as u64).to_be_bytes());
    for (kind, row_id, project_id) in stamps {
        field(&mut hasher, legacy_store_token(*kind).as_bytes());
        field(&mut hasher, row_id.as_bytes());
        field(&mut hasher, project_id.as_str().as_bytes());
    }
    hasher.update((appended.len() as u64).to_be_bytes());
    for entry in appended {
        field(
            &mut hasher,
            entry.legacy_path_binding_id.as_str().as_bytes(),
        );
        hasher.update(entry.inventory_epoch.to_be_bytes());
        field(&mut hasher, entry.source_store.as_bytes());
        field(&mut hasher, entry.source_row_id.as_bytes());
        match &entry.status {
            LegacyPathBindingStatus::Mapped { project_id, .. } => {
                field(&mut hasher, b"mapped");
                field(&mut hasher, project_id.as_str().as_bytes());
            }
            LegacyPathBindingStatus::Unscoped {} => field(&mut hasher, b"unscoped"),
            LegacyPathBindingStatus::Quarantined {} => field(&mut hasher, b"quarantined"),
        }
    }
    Sha256ValueV1::parse(hex::encode(hasher.finalize())).expect("code-owned digest is valid")
}

/// The owner token for a store kind. Inverse of
/// [`legacy_store_kind_from_token`]; both must stay exhaustive together.
pub fn legacy_store_token(kind: LegacyPathStoreKindV1) -> &'static str {
    match kind {
        LegacyPathStoreKindV1::Knowledge => "knowledge",
        LegacyPathStoreKindV1::Gap => "gap",
        LegacyPathStoreKindV1::Thread => "thread",
        LegacyPathStoreKindV1::Note => "note",
        LegacyPathStoreKindV1::Pin => "pin",
        LegacyPathStoreKindV1::Roadmap => "roadmap",
        LegacyPathStoreKindV1::Packet => "packet",
        LegacyPathStoreKindV1::Task => "task",
        LegacyPathStoreKindV1::Proposal => "proposal",
        LegacyPathStoreKindV1::SlackBinding => "slack",
        LegacyPathStoreKindV1::Whiteboard => "whiteboard",
        LegacyPathStoreKindV1::Artifact => "artifact",
        LegacyPathStoreKindV1::Provenance => "provenance",
        LegacyPathStoreKindV1::TranscriptEdge => "transcript-edge",
    }
}

/// Derive the appended binding's id from its content rather than minting a
/// random one.
///
/// `LegacyPathBindingId::mint()` is a UUID, which would make the plan hash
/// unpredictable and re-apply non-idempotent: a torn apply that already
/// appended a superseding binding would append a SECOND one on retry. Deriving
/// the id from the superseded binding plus the new epoch and status makes the
/// append converge on the same row, so re-apply is a no-op.
fn derive_appended_binding_id(
    superseded: &LegacyPathBindingId,
    inventory_epoch: u64,
    status: &LegacyPathBindingStatus,
) -> LegacyPathBindingId {
    let mut hasher = Sha256::new();
    field(
        &mut hasher,
        b"blackbox.project-catalog.backfill-appended-binding.v1",
    );
    field(&mut hasher, superseded.as_str().as_bytes());
    hasher.update(inventory_epoch.to_be_bytes());
    match status {
        LegacyPathBindingStatus::Mapped { project_id, .. } => {
            field(&mut hasher, b"mapped");
            field(&mut hasher, project_id.as_str().as_bytes());
        }
        LegacyPathBindingStatus::Unscoped {} => field(&mut hasher, b"unscoped"),
        LegacyPathBindingStatus::Quarantined {} => field(&mut hasher, b"quarantined"),
    }
    let digest = hex::encode(hasher.finalize());
    LegacyPathBindingId::parse(format!("lpb_{}", &digest[..32]))
        .expect("code-owned derived binding id is valid")
}

// ---------------------------------------------------------------------------
// Planning
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct BackfillPlanV1 {
    inventory_hash: Sha256ValueV1,
    plan_hash: Sha256ValueV1,
    rows: Vec<BackfillLedgerRowV1>,
    classification_counts: BTreeMap<LegacyPathStoreKindV1, LegacyStoreClassificationCountsV1>,
    planned_stamps: BTreeMap<LegacyPathStoreKindV1, u64>,
    unscoped_legacy_counts: BTreeMap<LegacyPathStoreKindV1, u64>,
    stamps: Vec<(LegacyPathStoreKindV1, String, ProjectId)>,
    appended: Vec<LegacyPathLedgerEntry>,
}

/// Build the deterministic plan from a predecessor pair and a resolution.
///
/// Called identically by preflight (with the canonical empty resolution, or the
/// operator's reviewed one) and by apply (with the exact reviewed one), so the
/// plan hash apply recomputes is produced by the same code path that wrote the
/// hash the report carries. A second planner would be free to drift.
fn plan_backfill(
    attachments: &AttachmentSnapshotV1,
    epoch: u64,
    catalog_hash: &Sha256ValueV1,
    attachment_hash: &Sha256ValueV1,
    resolution: &DurableBackfillResolutionV1,
) -> BackfillResult<BackfillPlanV1> {
    let effective = effective_legacy_bindings(&attachments.legacy_path_bindings)?;
    let inventory_hash = backfill_inventory_hash(epoch, catalog_hash, attachment_hash, &effective);
    if resolution.inventory_hash != inventory_hash {
        return Err(refuse(
            ERROR_STALE_RESOLUTION,
            "backfill resolution was captured against a different predecessor inventory",
        ));
    }

    // Index the effective bindings by the id the resolution names, so a
    // disposition can be validated against the binding it claims to resolve.
    let by_id = effective
        .values()
        .map(|binding| {
            (
                binding.effective.legacy_path_binding_id.clone(),
                &binding.effective,
            )
        })
        .collect::<BTreeMap<_, _>>();

    let mut seen_dispositions = BTreeSet::new();
    let mut appended = Vec::new();
    let next_epoch = effective
        .values()
        .map(|binding| binding.effective.inventory_epoch)
        .max()
        .unwrap_or(0)
        .saturating_add(1);
    for disposition in &resolution.legacy_path_dispositions {
        let binding_id = disposition.legacy_path_binding_id();
        if !seen_dispositions.insert(binding_id.clone()) {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "backfill resolution names one binding more than once",
            ));
        }
        let Some(entry) = by_id.get(binding_id) else {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "backfill resolution names a binding absent from the effective ledger",
            ));
        };
        // The only resolvable state is quarantine. Mapping or superseding a
        // non-quarantined binding is exactly the inconsistency section 7.3
        // names, and retaining quarantine on a row that is not quarantined is
        // the same claim in the other direction.
        if !matches!(entry.status, LegacyPathBindingStatus::Quarantined {}) {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "backfill resolution names a binding that is not quarantined",
            ));
        }
        if !disposition.appends() {
            continue;
        }
        let status = match disposition {
            LegacyPathDispositionV1::MapToProjectId {
                project_id,
                relationship,
                ..
            } => LegacyPathBindingStatus::Mapped {
                project_id: project_id.clone(),
                relationship: *relationship,
            },
            LegacyPathDispositionV1::ClassifyUnscopedByAppendedSupersession { .. } => {
                LegacyPathBindingStatus::Unscoped {}
            }
            LegacyPathDispositionV1::RetainQuarantine { .. } => unreachable!("filtered above"),
        };
        appended.push(LegacyPathLedgerEntry {
            legacy_path_binding_id: derive_appended_binding_id(binding_id, next_epoch, &status),
            // Key fields are copied verbatim: an appended binding supersedes
            // only when its key matches, so a rewritten key would silently
            // create a new row instead of superseding the quarantined one.
            historical_path: entry.historical_path.clone(),
            source_store: entry.source_store.clone(),
            source_row_id: entry.source_row_id.clone(),
            inventory_epoch: next_epoch,
            status,
        });
    }
    appended.sort_by(|left, right| {
        left.legacy_path_binding_id
            .as_str()
            .cmp(right.legacy_path_binding_id.as_str())
    });

    // Project the post-resolution ledger and classify from it, so the counts
    // the report publishes describe the state apply installs, not the state it
    // started from.
    let mut projected = attachments.legacy_path_bindings.clone();
    for entry in &appended {
        projected.insert(entry.legacy_path_binding_id.clone(), entry.clone());
    }
    let projected_effective = effective_legacy_bindings(&projected)?;

    let mut rows = Vec::new();
    let mut classification_counts: BTreeMap<_, LegacyStoreClassificationCountsV1> = BTreeMap::new();
    let mut planned_stamps: BTreeMap<_, u64> = BTreeMap::new();
    let mut unscoped_legacy_counts: BTreeMap<_, u64> = BTreeMap::new();
    let mut stamps = Vec::new();
    for binding in projected_effective.values() {
        let entry = &binding.effective;
        let Some(store_kind) = legacy_store_kind_from_token(&entry.source_store) else {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "legacy path ledger names a store outside the owner set",
            ));
        };
        // Q-E3b: the Provenance owner is exempt by construction. Its capture is
        // project-parameterized and emits only inventory-target rows, so it
        // cannot produce a legacy-selector observation and therefore cannot
        // produce a ledger binding. One appearing here means the predecessor
        // inventory is inconsistent - most likely an invalid capture
        // association - and neither stamping the row nor comparing it could
        // repair that, so the plan refuses before anything is written.
        if store_kind == LegacyPathStoreKindV1::Provenance {
            return Err(refuse(
                ERROR_RESOLUTION_INVALID,
                "legacy path ledger carries a provenance binding, which the \
                 exempt-by-construction provenance owner cannot produce",
            ));
        }
        let classification = LegacyRowClassificationV1::of(&entry.status);
        let counts = classification_counts.entry(store_kind).or_default();
        counts.superseded += binding.superseded.len() as u64;
        let project_id = match &entry.status {
            LegacyPathBindingStatus::Mapped { project_id, .. } => {
                counts.mappable += 1;
                *planned_stamps.entry(store_kind).or_default() += 1;
                stamps.push((store_kind, entry.source_row_id.clone(), project_id.clone()));
                Some(project_id.clone())
            }
            LegacyPathBindingStatus::Quarantined {} => {
                counts.quarantined += 1;
                None
            }
            LegacyPathBindingStatus::Unscoped {} => {
                counts.unscoped += 1;
                *unscoped_legacy_counts.entry(store_kind).or_default() += 1;
                None
            }
        };
        rows.push(BackfillLedgerRowV1 {
            legacy_path_binding_id: entry.legacy_path_binding_id.clone(),
            store_kind,
            historical_path_digest: digest_path(&entry.historical_path),
            source_row_id: entry.source_row_id.clone(),
            inventory_epoch: entry.inventory_epoch,
            classification,
            project_id,
            superseded_count: binding.superseded.len() as u64,
        });
    }
    stamps.sort();
    let plan_hash = backfill_plan_hash(&inventory_hash, &stamps, &appended);
    Ok(BackfillPlanV1 {
        inventory_hash,
        plan_hash,
        rows,
        classification_counts,
        planned_stamps,
        unscoped_legacy_counts,
        stamps,
        appended,
    })
}

// ---------------------------------------------------------------------------
// Publisher / G1 verification (D-040)
// ---------------------------------------------------------------------------

/// Verify installed publisher evidence per disposition. Never seeds either
/// branch (FD-1, section 3.3).
///
/// The dispositions come from the migration's own reviewed resolution, which
/// is the only place a project's publisher branch is chosen. This function
/// reads what the migration transaction installed and reports agreement or
/// defect; it writes nothing.
/// Exactly the five fields the publisher verification below reads, plus the
/// disposition split it branches on.
///
/// Purpose-built rather than reusing `PublisherBindingDispositionV1`, and that
/// is a correctness point rather than a style one: that type's `SeedG1` variant
/// demands `payload_hashes` and `accepted_commit`, which the migration marker
/// does not carry and this verification never reads. Routing marker evidence
/// through it would mean inventing hash values to satisfy a constructor, i.e.
/// fabricating data into a durable-evidence path. This type demands only what
/// the marker actually has and the verification actually uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackfillPublisherDispositionV1 {
    SeedG1 {
        project_id: ProjectId,
        expected_scope: PublishedScope,
        full_ref: String,
        generation_id: String,
        pointer_sha256: Sha256ValueV1,
    },
    NoPublishedContentAcknowledged {
        project_id: ProjectId,
        expected_scope: PublishedScope,
        full_ref: String,
    },
}

impl BackfillPublisherDispositionV1 {
    fn from_evidence(evidence: PublisherDispositionEvidenceV1) -> Self {
        match evidence {
            PublisherDispositionEvidenceV1::SeedG1 {
                project_id,
                expected_scope,
                full_ref,
                generation_id,
                pointer_sha256,
                ..
            } => Self::SeedG1 {
                project_id,
                expected_scope,
                full_ref: full_ref.to_string(),
                generation_id: generation_id.to_string(),
                pointer_sha256: Sha256ValueV1::parse(pointer_sha256.to_string())
                    .expect("marker evidence carries a validated sha256"),
            },
            PublisherDispositionEvidenceV1::NoPublishedContentAcknowledged {
                project_id,
                expected_scope,
                full_ref,
                ..
            } => Self::NoPublishedContentAcknowledged {
                project_id,
                expected_scope,
                full_ref: full_ref.to_string(),
            },
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

    fn full_ref(&self) -> &str {
        match self {
            Self::SeedG1 { full_ref, .. }
            | Self::NoPublishedContentAcknowledged { full_ref, .. } => full_ref,
        }
    }
}

/// Project the migration marker's retained evidence onto the five fields the
/// publisher verification reads.
///
/// A refusal here is the marker's own `error.project_catalog_migration_incomplete`,
/// surfaced rather than swallowed: a MigratedV1 target whose marker is missing
/// or disagrees with its transaction journal must REFUSE, never verify an empty
/// set and report a clean publisher check.
fn backfill_publisher_dispositions(
    store: &ProjectCatalogStore,
) -> BackfillResult<Vec<BackfillPublisherDispositionV1>> {
    Ok(store
        .migration_publisher_dispositions()
        .map_err(|error| refuse(error.code(), error.to_string()))?
        .into_iter()
        .map(BackfillPublisherDispositionV1::from_evidence)
        .collect())
}

/// One accepted-publication artifact's byte bound. A pointer and a generation
/// record are small JSON documents; the bound is what keeps an unbounded or
/// adversarial file a refusal rather than a read.
const MAX_ACCEPTED_PUBLICATION_PROBE_BYTES: usize = 1024 * 1024;

/// What a CONFINED probe of one accepted-publication artifact observed.
///
/// The two-way split is fail-closed by construction, and that is the whole
/// point. `Path::is_file()` collapses "it is not there" and "I could not tell"
/// into one `false`: a permission failure, an ENOTDIR, a symlink loop, or an
/// I/O error all read as PROVEN ABSENCE. On the
/// `NoPublishedContentAcknowledged` branch that turned an unreadable pointer
/// into `PointerAbsentAsRequired`, i.e. a D-040 proof passed on the strength of
/// an error. Here only an ENOENT - of the artifact, or of the directory that
/// would contain it - is absence; everything else refuses.
enum PublicationProbeV1 {
    /// The artifact is a regular file, opened NOFOLLOW and read bounded.
    Present(Vec<u8>),
    /// The artifact is EXACTLY absent.
    Absent,
}

/// Probe one accepted-publication artifact under `root`, failing closed on
/// anything that is not an exact absence.
///
/// The bytes come back from the SAME open that proved presence, so a `SeedG1`
/// hash check reads what it just proved rather than re-resolving the path.
fn probe_publication_artifact(
    root: &Path,
    name: &str,
    label: &'static str,
) -> BackfillResult<PublicationProbeV1> {
    let directory = NofollowDirectory::open_existing(root).map_err(|_| {
        refuse(
            ERROR_STALE_POST_IMAGE,
            format!("the {label} root is unsafe or unreadable"),
        )
    })?;
    // An absent containing directory IS an exact absence of everything under
    // it: the open distinguishes ENOENT from every other failure.
    let Some(directory) = directory else {
        return Ok(PublicationProbeV1::Absent);
    };
    match directory.read_regular(name, MAX_ACCEPTED_PUBLICATION_PROBE_BYTES, label) {
        Ok(Some(bytes)) => Ok(PublicationProbeV1::Present(bytes)),
        Ok(None) => Ok(PublicationProbeV1::Absent),
        Err(_) => Err(refuse(
            ERROR_STALE_POST_IMAGE,
            format!("the {label} is unsafe, oversize, or unreadable"),
        )),
    }
}

pub fn verify_publisher_dispositions(
    dispositions: &[BackfillPublisherDispositionV1],
    pointers_root: &Path,
    generations_root: &Path,
) -> BackfillResult<Vec<PublisherDispositionVerificationV1>> {
    let mut rows = Vec::new();
    for disposition in dispositions {
        let project_id = disposition.project_id().clone();
        let pointer = probe_publication_artifact(
            pointers_root,
            &format!("{}.json", project_id.as_str()),
            "accepted-publication pointer",
        )?;
        let expected_scope_digest = digest_published_scope(disposition.expected_scope())
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let full_ref_digest = digest_publisher_full_ref(disposition.full_ref())
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let (kind, outcome) = match disposition {
            BackfillPublisherDispositionV1::SeedG1 {
                generation_id,
                pointer_sha256,
                ..
            } => {
                // MIRRORS `AcceptedPublicationStorePaths::generation`:
                // `<generations>/<project_id>/<generation_id>.json`. An earlier
                // revision joined the generation id straight onto the root,
                // which matches no file the store ever writes, so EVERY SeedG1
                // disposition reported GenerationMissing and every preflight
                // against a real migrated root refused. Re-derived here rather
                // than called because that helper needs the whole paths struct
                // and a typed id; the test below pins the two shapes together
                // so this copy cannot drift from the store's own layout.
                let generation = probe_publication_artifact(
                    &generations_root.join(project_id.as_str()),
                    &format!("{generation_id}.json"),
                    "accepted-publication generation",
                )?;
                let outcome = match (&pointer, &generation) {
                    (PublicationProbeV1::Absent, _) => {
                        PublisherVerificationOutcomeV1::PointerMissing
                    }
                    (_, PublicationProbeV1::Absent) => {
                        PublisherVerificationOutcomeV1::GenerationMissing
                    }
                    (PublicationProbeV1::Present(bytes), PublicationProbeV1::Present(_)) => {
                        if &Sha256ValueV1::digest(bytes) == pointer_sha256 {
                            PublisherVerificationOutcomeV1::PointerPresentAndConsistent
                        } else {
                            PublisherVerificationOutcomeV1::PointerHashMismatch
                        }
                    }
                };
                (PublisherDispositionKindV1::SeedG1, outcome)
            }
            BackfillPublisherDispositionV1::NoPublishedContentAcknowledged { .. } => {
                // D-040: such a project must have NO accepted-publication
                // pointer until an explicit Establish. A pointer found here is
                // a defect, not a gap this command may fill.
                let outcome = match pointer {
                    PublicationProbeV1::Present(_) => {
                        PublisherVerificationOutcomeV1::PointerUnexpectedlyPresent
                    }
                    PublicationProbeV1::Absent => {
                        PublisherVerificationOutcomeV1::PointerAbsentAsRequired
                    }
                };
                (
                    PublisherDispositionKindV1::NoPublishedContentAcknowledged,
                    outcome,
                )
            }
        };
        rows.push(PublisherDispositionVerificationV1 {
            project_id,
            kind,
            expected_scope_digest,
            full_ref_digest,
            outcome,
        });
    }
    rows.sort_by(|left, right| left.project_id.as_str().cmp(right.project_id.as_str()));
    Ok(rows)
}

fn publisher_outcome_token(outcome: PublisherVerificationOutcomeV1) -> &'static str {
    match outcome {
        PublisherVerificationOutcomeV1::PointerPresentAndConsistent => "pointer-present",
        PublisherVerificationOutcomeV1::PointerMissing => "pointer-missing",
        PublisherVerificationOutcomeV1::PointerHashMismatch => "pointer-hash-mismatch",
        PublisherVerificationOutcomeV1::GenerationMissing => "generation-missing",
        PublisherVerificationOutcomeV1::PointerAbsentAsRequired => "pointer-absent",
        PublisherVerificationOutcomeV1::PointerUnexpectedlyPresent => "pointer-unexpected",
    }
}

/// Canonical digest over a COMPLETE publisher verification.
///
/// This is the stable stamp that closes the preflight/apply race. Accepted
/// publication state is not part of the four-hash inventory - the inventory
/// hashes the ledger and the pair, which a publisher Establish does not touch -
/// so nothing in the four-hash identity would notice a publication appearing or
/// vanishing between preflight and apply. Apply therefore re-proves the whole
/// disposition set against the target and refuses on any difference from the
/// reviewed report, and records THIS digest in the completion journal so a later
/// verify proves the state it observes is the state apply committed against
/// rather than merely a state that happens to be defect-free.
fn publisher_verification_digest(rows: &[PublisherDispositionVerificationV1]) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    field(
        &mut hasher,
        b"blackbox.project-catalog.backfill-publisher-verification.v1",
    );
    hasher.update((rows.len() as u64).to_be_bytes());
    for row in rows {
        field(&mut hasher, row.project_id.as_str().as_bytes());
        field(
            &mut hasher,
            match row.kind {
                PublisherDispositionKindV1::SeedG1 => b"seed-g1".as_slice(),
                PublisherDispositionKindV1::NoPublishedContentAcknowledged => {
                    b"no-published-content-acknowledged".as_slice()
                }
            },
        );
        field(&mut hasher, row.expected_scope_digest.as_str().as_bytes());
        field(&mut hasher, row.full_ref_digest.as_str().as_bytes());
        field(&mut hasher, publisher_outcome_token(row.outcome).as_bytes());
    }
    Sha256ValueV1::parse(hex::encode(hasher.finalize())).expect("code-owned digest is valid")
}

/// Re-prove the COMPLETE publisher/G1 disposition against the target's live
/// accepted-publication state, and bind it to the reviewed report.
///
/// Called at BOTH of apply's publication boundaries and, without a report, by
/// verify. Preflight's verdict alone cannot authorize an apply: the check reads
/// files outside the catalog pair, so a publisher Establish or advance landing
/// between the two commands is invisible to every hash the report carries.
fn revalidate_publisher_dispositions(
    store: &ProjectCatalogStore,
    pointers_root: &Path,
    generations_root: &Path,
    report: &DurableBackfillReportV1,
) -> BackfillResult<Vec<PublisherDispositionVerificationV1>> {
    let dispositions = backfill_publisher_dispositions(store)?;
    let observed = verify_publisher_dispositions(&dispositions, pointers_root, generations_root)?;
    if observed.iter().any(|row| row.outcome.is_defect()) {
        return Err(refuse(
            ERROR_STALE_POST_IMAGE,
            "the target's accepted-publication evidence no longer satisfies its \
             migration publisher dispositions",
        ));
    }
    // Equality over the COMPLETE set, not merely absence of defects: a
    // disposition that changed kind, scope, or ref between preflight and apply
    // is a different population from the one the operator reviewed even when
    // both verify cleanly.
    if observed != report.publisher_verification {
        return Err(refuse(
            ERROR_STALE_POST_IMAGE,
            "the accepted-publication state moved after the reviewed report was captured",
        ));
    }
    Ok(observed)
}

// ---------------------------------------------------------------------------
// Requests and facade
// ---------------------------------------------------------------------------

/// Preflight against ONE explicit target layout.
///
/// There is no rehearsal-versus-protected duality in the request and no
/// separation check inside the facade: isolation is the CALLER's
/// responsibility, expressed by which layout it hands in. A rehearsal apply is
/// an apply against a layout derived from the isolated root, and needs no
/// exclusive lock because the pair transaction's own locks provide correctness
/// inside that root (section 4.3).
pub struct DurableBackfillPreflightRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    /// Which target the CLI resolved. Bound to `layout`'s actual shape before
    /// any other observable work (adjudication Q-C).
    pub target_selection: ProjectCatalogTargetSelectionV1,
    /// OUTPUT path. Preflight writes the report here.
    pub report_path: PathBuf,
    /// OUTPUT path. A first preflight may create the canonical empty
    /// resolution here; an existing reviewed resolution is read and honoured.
    pub resolution_path: PathBuf,
    /// Probed for per-store coverage, never invoked to write. Preflight writes
    /// no project state.
    pub stamper: Arc<dyn LegacyRowStamperV1>,
    /// Timestamp source. Passed in rather than read from the clock so the
    /// facade stays deterministic under test.
    pub generated_at: String,
}

pub struct DurableBackfillApplyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    /// Which target the CLI resolved. Bound to `layout`'s actual shape before
    /// any other observable work (adjudication Q-C).
    pub target_selection: ProjectCatalogTargetSelectionV1,
    /// INPUT path: the exact reviewed report.
    pub report_path: PathBuf,
    /// INPUT path: the exact reviewed resolution.
    pub resolution_path: PathBuf,
    pub stamper: Arc<dyn LegacyRowStamperV1>,
    pub completed_at: String,
}

/// Verify takes no artifacts (section 3.1): it runs fresh verification against
/// durable state, which already carries the artifact hashes it was applied
/// from.
pub struct DurableBackfillVerifyRequestV1 {
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    /// Which target the CLI resolved. Verify is target-explicit under the same
    /// selection rules as apply (section 3.1).
    pub target_selection: ProjectCatalogTargetSelectionV1,
    /// Reads the durable owner rows the applied plan claims to have stamped.
    /// Injected for the same reason the stamper is: the fourteen owners live in
    /// their own crates and only the root crate sees them all.
    pub owner_reader: Arc<dyn LegacyRowOwnerReaderV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DurableBackfillPreflightReceiptV1 {
    pub version: u32,
    pub status: DurableBackfillStatusV1,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    pub predecessor_catalog_epoch: u64,
    pub predicted_post_image_catalog_epoch: u64,
    pub planned_stamp_total: u64,
    pub quarantined_total: u64,
    pub unscoped_total: u64,
    pub publisher_defect_count: u64,
    pub uncovered_store_count: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DurableBackfillApplyOutcomeV1 {
    Applied,
    AlreadyApplied,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DurableBackfillApplyReceiptV1 {
    pub version: u32,
    pub outcome: DurableBackfillApplyOutcomeV1,
    pub journal: BackfillCompletionJournalV1,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DurableBackfillVerifyReceiptV1 {
    pub version: u32,
    pub predecessor_catalog_epoch: u64,
    pub observed_catalog_epoch: u64,
    pub journal_stamp_total: u64,
    pub observed_mappable_total: u64,
    pub quarantined_total: u64,
    pub unscoped_total: u64,
}

/// The only public executable durable-backfill authority.
pub struct ProjectCatalogDurableBackfillFacadeV1;

impl ProjectCatalogDurableBackfillFacadeV1 {
    /// Capture the predecessor under the SHARED lifetime lock and plan.
    ///
    /// Preflight writes no project state. It holds the shared lifetime lock
    /// DIRECTLY, for the store's whole lifetime, and deliberately does NOT run
    /// inside `capture_migration_preflight_with`. That helper's two-lock pattern
    /// (shared lifetime plus exclusive store mutation) belongs to the
    /// MIGRATION's raw-file capture, which opens no version-2 store and so has
    /// no other source of pair-read coherence. A new-verb preflight whose
    /// capture OPENS the store cannot use it: `open_existing` re-acquires that
    /// same mutation lock on a second descriptor and self-deadlocks. Section
    /// 4.1's requirement is met by the open itself, and the pair read is
    /// coherent because it runs under the mutation lock the open takes; a shared
    /// lifetime lock does not exclude the daemon's own shared handle, so this
    /// runs against live configured state and still sees a consistent capture.
    /// The store-open comment below records the failure that settled this.
    pub fn preflight(
        request: DurableBackfillPreflightRequestV1,
    ) -> BackfillResult<DurableBackfillPreflightReceiptV1> {
        // Q-C binding conditions, both BEFORE any artifact access or store
        // read: the selected target must match the layout's real shape, and
        // the artifacts must be confined to that target.
        validate_target_selection(&request.layout, request.target_selection)?;
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )?;
        let projects_path = request.layout.projects_path().to_path_buf();
        let pointers_root = request.layout.accepted_publication_pointers_for_backfill();
        let generations_root = request
            .layout
            .accepted_publication_generations_for_backfill();

        // The capture takes its locks through the STORE OPEN, not through an
        // outer `capture_migration_preflight_with`. That is required, not a
        // shortcut: the helper holds the store MUTATION lock across its
        // closure, and `open_existing` acquires that same mutation lock itself
        // on a second descriptor, so the open blocks forever on this process's
        // own exclusive flock. Section 4.1's requirement (preflight runs under
        // the shared lifetime lock) is satisfied by the open, which holds that
        // lock for the store's whole lifetime, and the pair read is coherent
        // because `read_strict_pair_locked` runs under the mutation lock the
        // open takes for itself. Apply and verify below already open directly
        // for the same reason; this preflight was the odd one out.
        let store = ProjectCatalogStore::open_existing(&projects_path)
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let state = store
            .snapshot()
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let (epoch, catalog_hash, attachment_hash, attachments) = (
            state.epoch(),
            Sha256ValueV1::parse(state.catalog_sha256().to_string())
                .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?,
            Sha256ValueV1::parse(state.attachments_sha256().to_string())
                .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?,
            (**state.attachments()).clone(),
        );

        // A first preflight may CREATE the canonical empty resolution at the
        // explicit path (D-026); a reviewed one already there is honoured and
        // never rewritten. Its EXACT bytes are carried forward rather than a
        // re-encoding of the decoded value: those bytes are what the operator
        // reviewed and what apply will hash, so re-encoding would both destroy
        // the reviewed artifact and record a digest of bytes that never existed
        // on disk.
        let (resolution, resolution_bytes) =
            match read_backfill_artifact_optional(&request.resolution_path, RESOLUTION_LABEL)? {
                Some(bytes) => (decode_resolution(&bytes)?, bytes),
                None => {
                    let empty = DurableBackfillResolutionV1::empty(backfill_inventory_hash(
                        epoch,
                        &catalog_hash,
                        &attachment_hash,
                        &effective_legacy_bindings(&attachments.legacy_path_bindings)?,
                    ));
                    let bytes = encode_resolution(&empty)?;
                    (empty, bytes)
                }
            };
        let plan = plan_backfill(
            &attachments,
            epoch,
            &catalog_hash,
            &attachment_hash,
            &resolution,
        )?;
        // Dispositions come from the migration MARKER, not the request. The CLI
        // has no lawful source for them: the migration resolution artifact is
        // not on the backfill surface (FD-3) and receipts carry only a count.
        // The marker is their durable home, written at apply as a transaction
        // participant, journal-bound on every read, and scoped to exactly the
        // MigratedV1 population the backfill runs against. Deriving here also
        // means the facade reads them for the SAME target layout it already
        // validated, so no new path input enters the surface.
        let publisher_dispositions = backfill_publisher_dispositions(&store)?;
        let publisher_verification = verify_publisher_dispositions(
            &publisher_dispositions,
            &pointers_root,
            &generations_root,
        )?;
        let publisher_defect_count = publisher_verification
            .iter()
            .filter(|row| row.outcome.is_defect())
            .count() as u64;

        let resolution_artifact_hash = Sha256ValueV1::digest(&resolution_bytes);
        // The pair moves ONLY on an appended supersession. Mappable stamping
        // writes durable stores and leaves the catalog epoch alone, so a
        // conversion-free backfill genuinely predicts its own predecessor.
        let predicted_post_image_catalog_epoch = if plan.appended.is_empty() {
            epoch
        } else {
            epoch.saturating_add(1)
        };
        // Every owner, not just the planned ones. An exempt owner has zero
        // planned stamps by definition, so keying this by planned stores would
        // omit precisely the verdict the operator needs recorded (Q-E3b), and
        // an owner silently absent from the report reads as "not considered"
        // rather than "considered and exempt".
        let stamp_coverage = LegacyPathStoreKindV1::all()
            .into_iter()
            .map(|kind| (kind, request.stamper.coverage(kind)))
            .collect::<BTreeMap<_, _>>();
        let uncovered_store_count =
            uncovered_planned_stores(&plan.planned_stamps, request.stamper.as_ref()).len() as u64;
        // An exempt owner with a positive planned count is the exemption's
        // premise failing (Q-E3b). Counted into the same refusal so a broken
        // exemption can never publish a Clean report.
        let exempt_with_stamps_count =
            exempt_stores_with_planned_stamps(&plan.planned_stamps, request.stamper.as_ref()).len()
                as u64;
        // An owner with planned stamps and no write-back cannot be backfilled,
        // so the report refuses instead of publishing a plan that would stamp
        // a strict subset of what it promises. Apply's existing
        // refused-report guard then blocks the invocation.
        let status = if publisher_defect_count > 0
            || uncovered_store_count > 0
            || exempt_with_stamps_count > 0
        {
            DurableBackfillStatusV1::Refused
        } else {
            DurableBackfillStatusV1::Clean
        };
        let report = DurableBackfillReportV1 {
            version: DURABLE_BACKFILL_REPORT_VERSION_V1,
            generated_at: request.generated_at,
            status,
            inventory_hash: plan.inventory_hash.clone(),
            plan_hash: plan.plan_hash.clone(),
            resolution_artifact_hash: resolution_artifact_hash.clone(),
            predecessor_catalog_epoch: epoch,
            predecessor_catalog_hash: catalog_hash,
            predecessor_attachment_hash: attachment_hash,
            ledger_rows: plan.rows,
            classification_counts: plan.classification_counts,
            planned_stamps: plan.planned_stamps.clone(),
            unscoped_legacy_counts: plan.unscoped_legacy_counts.clone(),
            publisher_verification,
            stamp_coverage,
            predicted_post_image_catalog_epoch,
            completion_journal_relative_path: BACKFILL_COMPLETION_JOURNAL_FILE.to_string(),
        };
        let report_bytes = encode_report(&report)?;
        // The resolution lands FIRST, and only if absent. Ordered that way so a
        // published report always names a resolution that is on disk with
        // exactly the bytes the report's hash was taken over; the create-only
        // write doubles as the proof that nothing rewrote the reviewed
        // resolution between this preflight's read and its report.
        write_backfill_artifact_if_absent(
            &request.resolution_path,
            &resolution_bytes,
            RESOLUTION_LABEL,
        )?;
        // The report is preflight's OWN output: rerunning against fresh state
        // must replace it, and the replacement is atomic and confined.
        write_backfill_artifact_replacing(&request.report_path, &report_bytes, REPORT_LABEL)?;

        Ok(DurableBackfillPreflightReceiptV1 {
            version: DURABLE_BACKFILL_REPORT_VERSION_V1,
            status,
            inventory_hash: plan.inventory_hash,
            plan_hash: plan.plan_hash,
            report_artifact_hash: Sha256ValueV1::digest(&report_bytes),
            resolution_artifact_hash,
            predecessor_catalog_epoch: epoch,
            predicted_post_image_catalog_epoch,
            planned_stamp_total: plan.planned_stamps.values().sum(),
            quarantined_total: report
                .classification_counts
                .values()
                .map(|counts| counts.quarantined)
                .sum(),
            unscoped_total: plan.unscoped_legacy_counts.values().sum(),
            publisher_defect_count,
            uncovered_store_count,
        })
    }

    /// Install the reviewed post-image against the selected target.
    ///
    /// LOCK PRECONDITION, part of this entry's contract. Against a CONFIGURED
    /// target the caller must already hold the factored admin lifetime claim
    /// (`acquire_admin_lifetime_claim`: exclusive probe, then downgrade to
    /// shared) and must hold it across this ENTIRE call including post-commit
    /// verification. This facade never acquires that claim itself, because a
    /// claim taken and released inside one call would leave the pair
    /// transaction and the journal write in separate exclusion windows. Against
    /// a REHEARSAL target no configured-store lock is taken and no configured
    /// fallback exists: the target is whatever layout the caller resolved, and
    /// the pair transaction's own locks provide correctness inside an isolated
    /// root (section 4.3).
    ///
    /// Ordering is load-bearing and is what makes a torn apply recoverable:
    ///
    /// 1. re-derive the plan against the target's CURRENT state and refuse a
    ///    stale predecessor rather than replanning (FD-10, section 6.2);
    /// 2. commit any appended supersessions in ONE pair transaction;
    /// 3. stamp the mappable rows idempotently;
    /// 4. fsync the completion journal LAST.
    ///
    /// If the pair commit lands but stamping crashes midway, the journal is
    /// absent and the catalog epoch has moved, so re-apply refuses on the stale
    /// predecessor exactly as section 3.3 requires; recovery is a fresh
    /// preflight, review, and re-apply, whose stamps are idempotent over the
    /// partially stamped set and whose appended bindings are content-derived so
    /// the committed conversions are not duplicated.
    pub fn apply(
        request: DurableBackfillApplyRequestV1,
    ) -> BackfillResult<DurableBackfillApplyReceiptV1> {
        // Q-C binding conditions, both BEFORE the artifacts are read: an apply
        // whose artifacts live inside the target it is about to mutate, or
        // whose selection disagrees with the layout, is refused with nothing
        // touched.
        validate_target_selection(&request.layout, request.target_selection)?;
        validate_artifact_set(
            &request.layout,
            &request.report_path,
            &request.resolution_path,
            None,
        )?;
        let projects_path = request.layout.projects_path().to_path_buf();
        let state_dir = request.layout.state_dir_for_backfill().to_path_buf();
        let pointers_root = request.layout.accepted_publication_pointers_for_backfill();
        let generations_root = request
            .layout
            .accepted_publication_generations_for_backfill();

        let report_bytes = read_backfill_artifact_required(&request.report_path, REPORT_LABEL)?;
        let resolution_bytes =
            read_backfill_artifact_required(&request.resolution_path, RESOLUTION_LABEL)?;
        let report = decode_report(&report_bytes)?;
        let resolution = decode_resolution(&resolution_bytes)?;
        if report.status == DurableBackfillStatusV1::Refused {
            return Err(refuse(
                ERROR_ARTIFACT_IDENTITY,
                "backfill report records a refused status and cannot authorize an apply",
            ));
        }

        let store = ProjectCatalogStore::open_existing(&projects_path)
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let state = store
            .snapshot()
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let epoch = state.epoch();
        let catalog_hash = Sha256ValueV1::parse(state.catalog_sha256().to_string())
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let attachment_hash = Sha256ValueV1::parse(state.attachments_sha256().to_string())
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;

        let report_artifact_hash = Sha256ValueV1::digest(&report_bytes);
        let resolution_artifact_hash = Sha256ValueV1::digest(&resolution_bytes);
        // An already-applied backfill is recognised by its journal rather than
        // by re-deriving state, so a re-invocation is a cheap no-op instead of
        // a second stamping pass. The recognition is the COMPLETE identity, not
        // the report's byte hash alone: the journal names all four hashes and
        // the predecessor it was applied against, and the report names three of
        // them, so a match on the report alone would report AlreadyApplied for
        // an invocation whose RESOLUTION bytes are unrelated to the one that
        // actually ran, silently blessing an artifact pair that was never
        // applied together.
        if let Some(existing) = read_backfill_completion_journal(&state_dir)?
            && existing.identity.report_artifact_hash == report_artifact_hash
        {
            if existing.identity.resolution_artifact_hash != resolution_artifact_hash
                || report.resolution_artifact_hash != resolution_artifact_hash
                || existing.identity.inventory_hash != report.inventory_hash
                || existing.identity.plan_hash != report.plan_hash
                || existing.predecessor_catalog_epoch != report.predecessor_catalog_epoch
                || existing.predecessor_catalog_hash != report.predecessor_catalog_hash
                || existing.predecessor_attachment_hash != report.predecessor_attachment_hash
            {
                return Err(refuse(
                    ERROR_ARTIFACT_IDENTITY,
                    "the completion journal names this report but a different \
                     artifact identity, so this invocation was never applied",
                ));
            }
            return Ok(DurableBackfillApplyReceiptV1 {
                version: DURABLE_BACKFILL_REPORT_VERSION_V1,
                outcome: DurableBackfillApplyOutcomeV1::AlreadyApplied,
                journal: existing,
            });
        }

        if epoch != report.predecessor_catalog_epoch {
            return Err(refuse(
                ERROR_STALE_REPORT,
                "backfill report predecessor epoch does not match the target's current epoch",
            ));
        }
        let plan = plan_backfill(
            state.attachments(),
            epoch,
            &catalog_hash,
            &attachment_hash,
            &resolution,
        )?;
        // The four-hash identity recheck against the CURRENT target state. This
        // is the cross-root guard: a report captured against one root cannot
        // reproduce this inventory hash against any other.
        if plan.inventory_hash != report.inventory_hash
            || plan.plan_hash != report.plan_hash
            || resolution_artifact_hash != report.resolution_artifact_hash
            || catalog_hash != report.predecessor_catalog_hash
            || attachment_hash != report.predecessor_attachment_hash
        {
            return Err(refuse(
                ERROR_ARTIFACT_IDENTITY,
                "reviewed backfill artifacts disagree with the selected target's current state",
            ));
        }

        // Coverage is re-probed here, not merely trusted from the report: the
        // stamper instance handed to apply need not be the one preflight
        // probed, and this runs BEFORE the pair transaction so an uncovered
        // owner refuses with nothing mutated.
        let uncovered = uncovered_planned_stores(&plan.planned_stamps, request.stamper.as_ref());
        if !uncovered.is_empty() {
            return Err(refuse(
                ERROR_ARTIFACT_IDENTITY,
                format!(
                    "the supplied stamper cannot write these planned owners: {}",
                    uncovered.join(", ")
                ),
            ));
        }
        // Same placement, same reason (Q-E3b): re-probed against THIS stamper
        // and before the pair transaction, so a failed exemption refuses with
        // nothing mutated rather than part-way through.
        let exempt_with_stamps =
            exempt_stores_with_planned_stamps(&plan.planned_stamps, request.stamper.as_ref());
        if !exempt_with_stamps.is_empty() {
            return Err(refuse(
                ERROR_ARTIFACT_IDENTITY,
                format!(
                    "these owners are exempt by construction yet the plan stamps them: {}",
                    exempt_with_stamps.join(", ")
                ),
            ));
        }

        // The publisher/G1 disposition is re-proved against the TARGET before
        // anything is mutated. Preflight's verdict is not carried forward on
        // trust: it reads accepted-publication files that no hash in the report
        // covers, so an Establish or advance landing between the two commands
        // would otherwise go unnoticed. Refusing here costs nothing - the pair
        // transaction has not run.
        revalidate_publisher_dispositions(&store, &pointers_root, &generations_root, &report)?;

        // Step 2: the pair moves only for appended supersessions.
        let mut converted: BTreeMap<LegacyPathStoreKindV1, u64> = BTreeMap::new();
        let mut converted_binding_ids: BTreeSet<LegacyPathBindingId> = BTreeSet::new();
        let post_image_catalog_epoch = if plan.appended.is_empty() {
            epoch
        } else {
            let appended = plan.appended.clone();
            let commit = store
                .transact(epoch, |_catalog, attachments| {
                    for entry in &appended {
                        attachments
                            .legacy_path_bindings
                            .insert(entry.legacy_path_binding_id.clone(), entry.clone());
                    }
                    Ok(())
                })
                .map_err(|error| {
                    ProjectCatalogMigrationError::new(
                        ERROR_STALE_POST_IMAGE,
                        error.to_string(),
                        ProjectCatalogMigrationMutationDispositionV1::RecoveredToOldState,
                    )
                })?;
            for entry in &plan.appended {
                if let Some(kind) = legacy_store_kind_from_token(&entry.source_store) {
                    *converted.entry(kind).or_default() += 1;
                }
                // Recorded whatever the store token resolves to, so a journal
                // can never omit a committed conversion by naming an owner the
                // token map does not know.
                converted_binding_ids.insert(entry.legacy_path_binding_id.clone());
            }
            commit.epoch
        };

        // Step 3: stamp. Idempotent per row, so a retry over a partially
        // stamped set completes without duplication.
        let mut stamp_counts: BTreeMap<LegacyPathStoreKindV1, BackfillStoreStampCountsV1> =
            BTreeMap::new();
        for (kind, row_id, project_id) in &plan.stamps {
            let counts = stamp_counts.entry(*kind).or_default();
            counts.mappable += 1;
            match request
                .stamper
                .stamp(*kind, row_id, project_id)
                .map_err(|error| error.with_backfill_stamping_disposition())?
            {
                LegacyRowStampOutcomeV1::Stamped => counts.stamped += 1,
                LegacyRowStampOutcomeV1::AlreadyStamped => counts.already_stamped += 1,
            }
        }
        for (kind, count) in &converted {
            stamp_counts.entry(*kind).or_default().converted += count;
        }
        for (kind, count) in &plan.unscoped_legacy_counts {
            stamp_counts.entry(*kind).or_default().unscoped += count;
        }

        // THE FINAL PUBLICATION BOUNDARY. The set is proved once more, after the
        // last stamp and immediately before the journal is published, so the
        // digest the journal records is an observation of the state this apply
        // actually finished against rather than of the state it started from.
        // A refusal here is post-mutation by construction.
        let publisher_verification =
            revalidate_publisher_dispositions(&store, &pointers_root, &generations_root, &report)
                .map_err(|error| post_mutation_refusal(error.code, error.message))?;

        // Step 4: the journal, fsynced before apply returns.
        let journal = BackfillCompletionJournalV1 {
            version: BACKFILL_COMPLETION_JOURNAL_VERSION_V1,
            completed_at: request.completed_at,
            predecessor_catalog_epoch: epoch,
            predecessor_catalog_hash: catalog_hash,
            predecessor_attachment_hash: attachment_hash,
            post_image_catalog_epoch,
            stamp_counts,
            converted_binding_ids,
            identity: BackfillArtifactIdentityV1 {
                inventory_hash: plan.inventory_hash,
                plan_hash: plan.plan_hash,
                report_artifact_hash,
                resolution_artifact_hash,
            },
            publisher_verification_digest: publisher_verification_digest(&publisher_verification),
        };
        write_backfill_completion_journal(&state_dir, &journal)?;
        Ok(DurableBackfillApplyReceiptV1 {
            version: DURABLE_BACKFILL_REPORT_VERSION_V1,
            outcome: DurableBackfillApplyOutcomeV1::Applied,
            journal,
        })
    }

    /// Fresh verification against durable state (task 6).
    ///
    /// Checks that the applied stamp set matches the completion journal and
    /// that the ledger is consistent under the section 3.3 supersession rule.
    /// It reads journals and the store, never operator artifacts: the durable
    /// records already carry the artifact hashes they were applied from (FD-4).
    pub fn verify(
        request: DurableBackfillVerifyRequestV1,
    ) -> BackfillResult<DurableBackfillVerifyReceiptV1> {
        // Verify takes no artifacts, so only the selection condition applies.
        validate_target_selection(&request.layout, request.target_selection)?;
        let projects_path = request.layout.projects_path().to_path_buf();
        let state_dir = request.layout.state_dir_for_backfill().to_path_buf();
        let pointers_root = request.layout.accepted_publication_pointers_for_backfill();
        let generations_root = request
            .layout
            .accepted_publication_generations_for_backfill();
        let Some(journal) = read_backfill_completion_journal(&state_dir)? else {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                "no backfill completion journal is present for this target",
            ));
        };
        let store = ProjectCatalogStore::open_existing(&projects_path)
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        let state = store
            .snapshot()
            .map_err(|error| refuse(ERROR_STALE_POST_IMAGE, error.to_string()))?;
        // The ledger must still resolve under the supersession rule. A ledger
        // that has since grown disagreeing bindings at one key and epoch fails
        // here rather than being read past.
        let effective = effective_legacy_bindings(&state.attachments().legacy_path_bindings)?;

        let mut observed_mappable_total = 0u64;
        let mut quarantined_total = 0u64;
        let mut unscoped_total = 0u64;
        let mut observed_per_store: BTreeMap<LegacyPathStoreKindV1, u64> = BTreeMap::new();
        let mut observed_unscoped_per_store: BTreeMap<LegacyPathStoreKindV1, u64> = BTreeMap::new();
        let mut expected_rows: BTreeMap<LegacyPathStoreKindV1, BTreeMap<String, ProjectId>> =
            BTreeMap::new();
        for binding in effective.values() {
            let Some(kind) = legacy_store_kind_from_token(&binding.effective.source_store) else {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "legacy path ledger names a store outside the owner set",
                ));
            };
            match &binding.effective.status {
                LegacyPathBindingStatus::Mapped { project_id, .. } => {
                    observed_mappable_total += 1;
                    *observed_per_store.entry(kind).or_default() += 1;
                    // The row this binding claims, and the id it must carry.
                    if expected_rows
                        .entry(kind)
                        .or_default()
                        .insert(binding.effective.source_row_id.clone(), project_id.clone())
                        .is_some()
                    {
                        return Err(refuse(
                            ERROR_STALE_POST_IMAGE,
                            "one owner row is bound by two effective mapped bindings",
                        ));
                    }
                }
                LegacyPathBindingStatus::Quarantined {} => quarantined_total += 1,
                LegacyPathBindingStatus::Unscoped {} => {
                    unscoped_total += 1;
                    *observed_unscoped_per_store.entry(kind).or_default() += 1;
                }
            }
        }
        // THE EXACT CONVERSION POPULATION, resolved from the DURABLE ledger.
        //
        // Each id the journal names must still be a real, effective, converted
        // binding, and the store it counts against is the LEDGER's, not the
        // journal's. That is what makes the per-store conversion counts below
        // exact rather than merely bounded: a journal that moved a conversion
        // from one owner to another - preserving the total, the epoch
        // transition, and every other count - now disagrees with the store the
        // ledger itself puts that binding in.
        let effective_by_id = effective
            .values()
            .map(|binding| (binding.effective.legacy_path_binding_id.clone(), binding))
            .collect::<BTreeMap<_, _>>();
        let mut journal_converted_per_store: BTreeMap<LegacyPathStoreKindV1, u64> = BTreeMap::new();
        for binding_id in &journal.converted_binding_ids {
            let Some(binding) = effective_by_id.get(binding_id) else {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "the journal names a conversion the ledger does not resolve as effective",
                ));
            };
            let Some(kind) = legacy_store_kind_from_token(&binding.effective.source_store) else {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "legacy path ledger names a store outside the owner set",
                ));
            };
            // A conversion appended a superseding binding over a QUARANTINED
            // predecessor at the same key, and resolved it to mapped or
            // unscoped. Anything else is not the row the journal claims.
            if matches!(
                binding.effective.status,
                LegacyPathBindingStatus::Quarantined {}
            ) {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "the journal names a conversion whose binding is still quarantined",
                ));
            }
            match binding.superseded.first() {
                Some(predecessor)
                    if matches!(predecessor.status, LegacyPathBindingStatus::Quarantined {}) => {}
                Some(_) => {
                    return Err(refuse(
                        ERROR_STALE_POST_IMAGE,
                        "the journal names a conversion that superseded a binding which was \
                         not quarantined",
                    ));
                }
                None => {
                    return Err(refuse(
                        ERROR_STALE_POST_IMAGE,
                        "the journal names a conversion that supersedes nothing",
                    ));
                }
            }
            *journal_converted_per_store.entry(kind).or_default() += 1;
        }

        // The journal's COMPLETE per-store classification, over the UNION of
        // every store any of the four records names. Iterating one side only is
        // how an omission hides: a store the journal never mentions is absent
        // from a journal-keyed loop, and a store with no MAPPED rows is absent
        // from a mappable-keyed one, so an unscoped-only store omitted from the
        // journal fell between the two and passed. Every store is compared
        // here, and an omission is read as zero only AFTER it has been compared
        // against what the ledger holds.
        let mut all_stores: BTreeSet<LegacyPathStoreKindV1> = BTreeSet::new();
        all_stores.extend(journal.stamp_counts.keys().copied());
        all_stores.extend(observed_per_store.keys().copied());
        all_stores.extend(observed_unscoped_per_store.keys().copied());
        all_stores.extend(journal_converted_per_store.keys().copied());
        for kind in all_stores {
            let counts = journal.stamp_counts.get(&kind).copied().unwrap_or_default();
            if counts.mappable != observed_per_store.get(&kind).copied().unwrap_or_default() {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "journal mappable counts disagree with the ledger's mappable set",
                ));
            }
            // Every mappable row must be accounted for as stamped or
            // already-stamped: a journal that recorded fewer describes a pass
            // that did not finish.
            if counts.stamped + counts.already_stamped != counts.mappable {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "journal stamp counts do not account for every mappable row",
                ));
            }
            if counts.unscoped
                != observed_unscoped_per_store
                    .get(&kind)
                    .copied()
                    .unwrap_or_default()
            {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "journal unscoped counts disagree with the ledger's unscoped set",
                ));
            }
            if counts.converted
                != journal_converted_per_store
                    .get(&kind)
                    .copied()
                    .unwrap_or_default()
            {
                return Err(refuse(
                    ERROR_STALE_POST_IMAGE,
                    "journal conversion counts disagree with the conversions the ledger \
                     puts in that store",
                ));
            }
        }
        // Conversions are exactly what moves the pair, so the epoch and the
        // converted total must agree in both directions.
        let converted_total: u64 = journal
            .stamp_counts
            .values()
            .map(|counts| counts.converted)
            .sum();
        if (converted_total > 0)
            != (journal.post_image_catalog_epoch != journal.predecessor_catalog_epoch)
        {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                "journal conversions disagree with the epoch the pair moved to",
            ));
        }
        if journal.post_image_catalog_epoch > state.epoch() {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                "journal post-image epoch is ahead of the target's current epoch",
            ));
        }

        // THE DURABLE OWNER PROOF. Everything above compares one record against
        // another; this is the only part that opens the stores the backfill
        // actually wrote and asks whether the rows are really stamped.
        for (kind, rows) in &expected_rows {
            match request.owner_reader.coverage(*kind) {
                LegacyRowStampCoverageV1::Covered => {}
                LegacyRowStampCoverageV1::NotImplemented => {
                    return Err(refuse(
                        ERROR_STALE_POST_IMAGE,
                        format!(
                            "{} carries mapped bindings but has no durable read-back to \
                             verify them",
                            legacy_store_token(*kind)
                        ),
                    ));
                }
                LegacyRowStampCoverageV1::ExemptByConstruction => {
                    return Err(refuse(
                        ERROR_STALE_POST_IMAGE,
                        format!(
                            "{} is exempt by construction yet carries mapped bindings",
                            legacy_store_token(*kind)
                        ),
                    ));
                }
            }
            let observed = request
                .owner_reader
                .observe(*kind, &rows.keys().cloned().collect())?;
            for (source_row_id, project_id) in rows {
                match observed.get(source_row_id) {
                    Some(LegacyRowObservationV1::StampedWith(observed_project_id))
                        if observed_project_id == project_id => {}
                    Some(LegacyRowObservationV1::StampedWith(_)) => {
                        return Err(refuse(
                            ERROR_STALE_POST_IMAGE,
                            format!(
                                "a {} row is stamped with a different project than its \
                                 effective mapped binding",
                                legacy_store_token(*kind)
                            ),
                        ));
                    }
                    Some(LegacyRowObservationV1::Unstamped) => {
                        return Err(refuse(
                            ERROR_STALE_POST_IMAGE,
                            format!(
                                "a {} row the journal reports stamped carries no stable \
                                 project id",
                                legacy_store_token(*kind)
                            ),
                        ));
                    }
                    Some(LegacyRowObservationV1::Absent) | None => {
                        return Err(refuse(
                            ERROR_STALE_POST_IMAGE,
                            format!(
                                "a {} row named by an effective mapped binding is absent \
                                 from its owner",
                                legacy_store_token(*kind)
                            ),
                        ));
                    }
                }
            }
        }

        // The publisher/G1 disposition is proved FRESH here too, against the
        // same marker-sourced population. Verify exists to answer "is this
        // target's applied backfill still sound", and a D-040 defect that
        // appeared after apply makes it unsound; a journal that recorded a
        // clean set says nothing about now.
        let publisher_dispositions = backfill_publisher_dispositions(&store)?;
        let observed_publisher_verification = verify_publisher_dispositions(
            &publisher_dispositions,
            &pointers_root,
            &generations_root,
        )?;
        if observed_publisher_verification
            .iter()
            .any(|row| row.outcome.is_defect())
        {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                "the target's accepted-publication evidence no longer satisfies its \
                 migration publisher dispositions",
            ));
        }
        // And it must be the SAME set apply committed against. Without the
        // journal's stamp this could only ask "is it clean now", which a
        // publication that appeared and was corrected would answer yes.
        if publisher_verification_digest(&observed_publisher_verification)
            != journal.publisher_verification_digest
        {
            return Err(refuse(
                ERROR_STALE_POST_IMAGE,
                "the accepted-publication state changed after the backfill was applied",
            ));
        }
        Ok(DurableBackfillVerifyReceiptV1 {
            version: DURABLE_BACKFILL_REPORT_VERSION_V1,
            predecessor_catalog_epoch: journal.predecessor_catalog_epoch,
            observed_catalog_epoch: state.epoch(),
            journal_stamp_total: journal.applied_stamp_total(),
            observed_mappable_total,
            quarantined_total,
            unscoped_total,
        })
    }
}

// ---------------------------------------------------------------------------
// Artifact IO
// ---------------------------------------------------------------------------

fn encode_report(report: &DurableBackfillReportV1) -> BackfillResult<Vec<u8>> {
    serde_json::to_vec(report).map_err(|error| {
        refuse(
            ERROR_STALE_REPORT,
            format!("report cannot be encoded: {error}"),
        )
    })
}

fn decode_report(bytes: &[u8]) -> BackfillResult<DurableBackfillReportV1> {
    let report: DurableBackfillReportV1 = serde_json::from_slice(bytes).map_err(|error| {
        refuse(
            ERROR_STALE_REPORT,
            format!("report is not a strict v1 document: {error}"),
        )
    })?;
    if report.version != DURABLE_BACKFILL_REPORT_VERSION_V1 {
        return Err(refuse(
            ERROR_STALE_REPORT,
            "unsupported backfill report version",
        ));
    }
    Ok(report)
}

fn encode_resolution(resolution: &DurableBackfillResolutionV1) -> BackfillResult<Vec<u8>> {
    serde_json::to_vec(resolution).map_err(|error| {
        refuse(
            ERROR_RESOLUTION_INVALID,
            format!("resolution cannot be encoded: {error}"),
        )
    })
}

/// Decode a reviewed resolution.
///
/// `deny_unknown_fields` plus the closed disposition enum is what makes the
/// absent delete disposition a REFUSAL rather than a silent no-op: an artifact
/// naming `acknowledged_delete` fails here with the section 7.3 code instead of
/// decoding to an empty disposition list.
fn decode_resolution(bytes: &[u8]) -> BackfillResult<DurableBackfillResolutionV1> {
    let resolution: DurableBackfillResolutionV1 =
        serde_json::from_slice(bytes).map_err(|error| {
            refuse(
                ERROR_RESOLUTION_INVALID,
                format!("resolution is not a strict v1 document: {error}"),
            )
        })?;
    if resolution.version != DURABLE_BACKFILL_RESOLUTION_VERSION_V1 {
        return Err(refuse(
            ERROR_RESOLUTION_INVALID,
            "unsupported backfill resolution version",
        ));
    }
    Ok(resolution)
}

/// D-026 artifact discipline, borrowed rather than restated.
///
/// These four wrappers exist only to bind the backfill's byte bound and its
/// artifact labels to the migration's confined artifact helpers. Those helpers
/// are the ones D-026 describes: they resolve the parent through a NOFOLLOW
/// directory handle, refuse a non-regular or symlinked artifact, read BOUNDED
/// (never allocating the whole file before checking its size), replace
/// atomically under an exclusive parent lock, and re-prove the parent was not
/// swapped mid-operation. The local `std::fs::read` / `std::fs::write` pair
/// they replace had none of that: it allocated first and bounded second, it
/// followed symlinks, and a partial write left a torn artifact behind.
const REPORT_LABEL: &str = "durable backfill report";
const RESOLUTION_LABEL: &str = "durable backfill resolution";

fn read_backfill_artifact_required(path: &Path, label: &'static str) -> BackfillResult<Vec<u8>> {
    crate::project_catalog_migration::read_artifact_required(
        path,
        MAX_BACKFILL_ARTIFACT_BYTES,
        label,
    )
}

fn read_backfill_artifact_optional(
    path: &Path,
    label: &'static str,
) -> BackfillResult<Option<Vec<u8>>> {
    crate::project_catalog_migration::read_artifact_optional(
        path,
        MAX_BACKFILL_ARTIFACT_BYTES,
        label,
    )
}

/// Create the artifact, or accept it unchanged. Byte-different existing content
/// REFUSES: a reviewed resolution is the operator's, not preflight's to rewrite.
fn write_backfill_artifact_if_absent(
    path: &Path,
    bytes: &[u8],
    label: &'static str,
) -> BackfillResult<()> {
    crate::project_catalog_migration::write_artifact_if_absent(
        path,
        bytes,
        MAX_BACKFILL_ARTIFACT_BYTES,
        label,
    )
}

fn write_backfill_artifact_replacing(
    path: &Path,
    bytes: &[u8],
    label: &'static str,
) -> BackfillResult<()> {
    crate::project_catalog_migration::write_artifact_replacing(
        path,
        bytes,
        MAX_BACKFILL_ARTIFACT_BYTES,
        label,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use bbox_corpus_core::identity::PublishedScope;

    /// REGRESSION: a preflight that reaches the capture must RETURN.
    ///
    /// An earlier revision wrapped the capture in
    /// `capture_migration_preflight_with`, which holds the store mutation lock
    /// across its closure, while `ProjectCatalogStore::open_existing` acquires
    /// that same lock on a second descriptor. The open blocked forever on this
    /// process's own exclusive flock: a deadlock, not a slow path. No unit test
    /// caught it because every other test refuses before the capture, and no
    /// end-to-end driver of this facade existed at all.
    ///
    /// The assertion is a BOUNDED WATCHDOG rather than "the test eventually
    /// passes". A regression here is a hang, so a test that merely called
    /// preflight would itself hang and depend on the harness hard-kill to
    /// report - turning a bug into a two-minute gate stall. This fails in
    /// seconds instead.
    ///
    /// What preflight returns is irrelevant here; reaching ANY answer proves
    /// the capture returned. It refuses, because a store with no migration
    /// marker has no publisher dispositions to verify.
    #[test]
    fn a_preflight_that_reaches_the_capture_returns_rather_than_deadlocking() {
        use std::sync::mpsc;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("live").join("projects.json");
        std::fs::create_dir_all(projects_path.parent().unwrap()).unwrap();
        // Dropped immediately: the deadlock is this process re-locking a file
        // nothing else holds, so the fixture must leave no lock behind or the
        // test would pass for the wrong reason.
        drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());

        let layout = test_layout(&root);
        let report_path = root.join("review").join("report.json");
        let resolution_path = root.join("review").join("resolution.json");

        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || {
            let outcome = ProjectCatalogDurableBackfillFacadeV1::preflight(
                DurableBackfillPreflightRequestV1 {
                    layout,
                    target_selection: ProjectCatalogTargetSelectionV1::Configured,
                    report_path,
                    resolution_path,
                    stamper: Arc::new(PartialStamper),
                    generated_at: "2026-08-05T00:00:00Z".to_string(),
                },
            );
            // The VERDICT is irrelevant; returning one at all is the proof.
            let _ = sender.send(
                outcome
                    .map(|receipt| receipt.status)
                    .map_err(|error| error.code),
            );
        });

        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect(
                "durable-backfill preflight deadlocked: its capture must not run inside a helper \
                 that already holds the store mutation lock, because open_existing re-acquires \
                 that lock on a second descriptor",
            );
        // It refuses, because this store has no migration marker to source
        // publisher dispositions from. See the dedicated test below for why
        // that refusal is the required behaviour rather than an empty set.
        assert_eq!(outcome, Err("error.project_catalog_migration_incomplete"));
    }

    /// A target with no readable migration marker REFUSES, and never verifies
    /// an empty disposition set.
    ///
    /// This is the silent pass the publisher check exists to prevent: an empty
    /// set verifies vacuously, so a store whose marker is missing or corrupt
    /// would report a clean publisher verification and a Clean backfill report.
    /// The refusal reuses the marker's own code rather than minting a new one.
    #[test]
    fn a_target_without_a_migration_marker_refuses_rather_than_verifying_nothing() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("live").join("projects.json");
        std::fs::create_dir_all(projects_path.parent().unwrap()).unwrap();
        drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());
        assert!(
            !root
                .join("live")
                .join("project-catalog-migration.json")
                .exists(),
            "the fixture must genuinely lack a marker"
        );

        let refused =
            ProjectCatalogDurableBackfillFacadeV1::preflight(DurableBackfillPreflightRequestV1 {
                layout: test_layout(&root),
                target_selection: ProjectCatalogTargetSelectionV1::Configured,
                report_path: root.join("review").join("report.json"),
                resolution_path: root.join("review").join("resolution.json"),
                stamper: Arc::new(PartialStamper),
                generated_at: "2026-08-05T00:00:00Z".to_string(),
            })
            .expect_err("a target with no marker has no lawful disposition source");

        assert_eq!(refused.code, "error.project_catalog_migration_incomplete");
        assert!(
            !root.join("review").join("report.json").exists(),
            "a refused preflight must not publish a report"
        );
    }

    /// D-026: the reviewed artifacts are read through the CONFINED BOUNDED
    /// helper, so a symlinked artifact and an oversize one both refuse.
    ///
    /// The local `std::fs::read` this replaced did neither. It followed the
    /// link, reading whatever the link pointed at as though it were the
    /// reviewed artifact, and it allocated the entire file into memory before
    /// comparing its length against the bound - so the bound documented an
    /// intent the code did not have. The refusals land before the marker check,
    /// which is why this fixture needs no migration marker.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_or_oversize_reviewed_resolution_refuses() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let projects_path = root.join("live").join("projects.json");
        std::fs::create_dir_all(projects_path.parent().unwrap()).unwrap();
        drop(ProjectCatalogStore::initialize_empty(&projects_path).unwrap());
        let review = root.join("review");
        std::fs::create_dir_all(&review).unwrap();

        let preflight = || {
            ProjectCatalogDurableBackfillFacadeV1::preflight(DurableBackfillPreflightRequestV1 {
                layout: test_layout(&root),
                target_selection: ProjectCatalogTargetSelectionV1::Configured,
                report_path: review.join("report.json"),
                resolution_path: review.join("resolution.json"),
                stamper: Arc::new(PartialStamper),
                generated_at: "2026-08-05T00:00:00Z".to_string(),
            })
        };

        // A LINK where the reviewed resolution should be. Following it would
        // read an artifact the operator never placed at the explicit path.
        std::fs::write(review.join("elsewhere.json"), b"{\"version\":1}").unwrap();
        std::os::unix::fs::symlink(
            review.join("elsewhere.json"),
            review.join("resolution.json"),
        )
        .unwrap();
        assert_eq!(
            preflight()
                .expect_err("a symlinked artifact is not a reviewed artifact")
                .code,
            "error.project_catalog_migration_artifact_io"
        );
        std::fs::remove_file(review.join("resolution.json")).unwrap();

        // And one byte past the bound refuses on the bound, having read at most
        // one byte beyond it.
        std::fs::write(
            review.join("resolution.json"),
            vec![b'x'; MAX_BACKFILL_ARTIFACT_BYTES + 1],
        )
        .unwrap();
        assert_eq!(
            preflight()
                .expect_err("an oversize artifact is refused, never decoded")
                .code,
            "error.project_catalog_migration_artifact_io"
        );
    }

    /// Mirrors `project_catalog_rebuild_planning.rs`'s helper of the same name.
    ///
    /// `vectors_dir` is explicit and load-bearing: the vector root defaults to
    /// the PLATFORM state directory (R33F1), so a fixture that omitted it would
    /// reach the host's real vector store.
    fn test_layout(root: &Path) -> ProjectCatalogMigrationResolvedLayoutV1 {
        let _guard = bbox_util::util::test_env_lock();
        let config_path = root.join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                root.join("live"),
                root.join("live").join("vectors")
            ),
        )
        .unwrap();
        let config = bbox_config::config::load_with(bbox_config::config::LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap();
        ProjectCatalogMigrationResolvedLayoutV1::from_config(
            &config,
            crate::project_catalog_migration::ProjectCatalogMigrationLayoutOverridesV1 {
                state_dir: Some(root.join("live")),
                projects_path: Some(root.join("live").join("projects.json")),
            },
        )
        .unwrap()
    }

    fn binding_id(seed: &str) -> LegacyPathBindingId {
        let mut hex = seed.to_string();
        while hex.len() < 32 {
            hex.push('0');
        }
        LegacyPathBindingId::parse(format!("lpb_{hex}")).unwrap()
    }

    fn project(seed: char) -> ProjectId {
        ProjectId::parse(format!(
            "p_{}",
            std::iter::repeat_n(seed, 32).collect::<String>()
        ))
        .unwrap()
    }

    fn entry(
        id: &str,
        epoch: u64,
        status: LegacyPathBindingStatus,
        store: &str,
    ) -> LegacyPathLedgerEntry {
        LegacyPathLedgerEntry {
            legacy_path_binding_id: binding_id(id),
            historical_path: "/host/checkouts/alpha".to_string(),
            source_store: store.to_string(),
            source_row_id: "row-alpha".to_string(),
            inventory_epoch: epoch,
            status,
        }
    }

    fn ledger(
        entries: Vec<LegacyPathLedgerEntry>,
    ) -> BTreeMap<LegacyPathBindingId, LegacyPathLedgerEntry> {
        entries
            .into_iter()
            .map(|entry| (entry.legacy_path_binding_id.clone(), entry))
            .collect()
    }

    fn attachments_with(entries: Vec<LegacyPathLedgerEntry>) -> AttachmentSnapshotV1 {
        let mut snapshot = AttachmentSnapshotV1::empty(7).unwrap();
        snapshot.legacy_path_bindings = ledger(entries);
        snapshot
    }

    fn hash(seed: u8) -> Sha256ValueV1 {
        Sha256ValueV1::digest(&[seed])
    }

    fn mapped(project_id: ProjectId) -> LegacyPathBindingStatus {
        LegacyPathBindingStatus::Mapped {
            project_id,
            relationship: LegacyPathRelationship::Root,
        }
    }

    /// The section 3.3 supersession rule: for equal
    /// (historical_path, source_store, source_row_id), the highest
    /// inventory_epoch wins and the loser is retained as an audit record.
    #[test]
    fn highest_inventory_epoch_supersedes_under_one_key() {
        let bindings = ledger(vec![
            entry(
                "a1",
                1,
                LegacyPathBindingStatus::Quarantined {},
                "knowledge",
            ),
            entry("b2", 4, mapped(project('a')), "knowledge"),
            entry("c3", 2, LegacyPathBindingStatus::Unscoped {}, "knowledge"),
        ]);
        let effective = effective_legacy_bindings(&bindings).unwrap();
        assert_eq!(effective.len(), 1, "one key resolves to one binding");
        let resolved = effective.values().next().unwrap();
        assert_eq!(resolved.effective.inventory_epoch, 4);
        assert!(matches!(
            resolved.effective.status,
            LegacyPathBindingStatus::Mapped { .. }
        ));
        assert_eq!(
            resolved.superseded.len(),
            2,
            "superseded bindings are retained as audit records, never dropped"
        );
    }

    /// A tie is not broken by iteration order when the statuses disagree: that
    /// ledger is inconsistent and refuses rather than picking a winner.
    #[test]
    fn disagreeing_bindings_at_one_key_and_epoch_refuse() {
        let bindings = ledger(vec![
            entry(
                "a1",
                3,
                LegacyPathBindingStatus::Quarantined {},
                "knowledge",
            ),
            entry("b2", 3, LegacyPathBindingStatus::Unscoped {}, "knowledge"),
        ]);
        let error = effective_legacy_bindings(&bindings).unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// Mappable stamping writes durable stores, not the pair, so a
    /// conversion-free backfill predicts its own predecessor epoch.
    #[test]
    fn a_conversion_free_plan_appends_nothing() {
        let attachments = attachments_with(vec![entry("a1", 1, mapped(project('a')), "knowledge")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let plan = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap();
        assert!(plan.appended.is_empty(), "no conversion, no pair mutation");
        assert_eq!(plan.stamps.len(), 1);
        assert_eq!(plan.planned_stamps[&LegacyPathStoreKindV1::Knowledge], 1);
    }

    /// Quarantine rides forward as counted state and is never stamped.
    #[test]
    fn unresolved_quarantine_is_counted_and_never_stamped() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "thread",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let plan = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap();
        assert!(plan.stamps.is_empty(), "quarantine never stamps");
        assert_eq!(
            plan.classification_counts[&LegacyPathStoreKindV1::Thread].quarantined,
            1
        );
    }

    /// Unscoped rows are counted by store and never stamped.
    #[test]
    fn unscoped_rows_are_counted_by_store_and_never_stamped() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Unscoped {},
            "pin",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let plan = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap();
        assert!(plan.stamps.is_empty());
        assert_eq!(plan.unscoped_legacy_counts[&LegacyPathStoreKindV1::Pin], 1);
    }

    /// map-to-project-id appends a SUPERSEDING binding at a higher epoch with
    /// matching key fields; the quarantined original is retained.
    #[test]
    fn map_to_project_id_appends_a_superseding_binding() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "gap",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![LegacyPathDispositionV1::MapToProjectId {
                legacy_path_binding_id: binding_id("a1"),
                project_id: project('b'),
                relationship: LegacyPathRelationship::Root,
            }],
            operator_notes: Vec::new(),
        };
        let plan = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap();
        assert_eq!(plan.appended.len(), 1);
        let appended = &plan.appended[0];
        assert_eq!(
            appended.inventory_epoch, 2,
            "appended above the predecessor"
        );
        assert_eq!(appended.source_store, "gap");
        assert_eq!(appended.source_row_id, "row-alpha");
        assert_eq!(appended.historical_path, "/host/checkouts/alpha");
        assert!(matches!(
            appended.status,
            LegacyPathBindingStatus::Mapped { .. }
        ));
        // The converted row now stamps, proving the plan classifies from the
        // POST-resolution ledger rather than the predecessor.
        assert_eq!(plan.stamps.len(), 1);
        assert_eq!(plan.stamps[0].2, project('b'));
    }

    /// classify-unscoped-by-appended-supersession appends `Unscoped {}` and
    /// still does not stamp.
    #[test]
    fn classify_unscoped_appends_without_stamping() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "note",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![
                LegacyPathDispositionV1::ClassifyUnscopedByAppendedSupersession {
                    legacy_path_binding_id: binding_id("a1"),
                },
            ],
            operator_notes: Vec::new(),
        };
        let plan = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap();
        assert_eq!(plan.appended.len(), 1);
        assert!(matches!(
            plan.appended[0].status,
            LegacyPathBindingStatus::Unscoped {}
        ));
        assert!(plan.stamps.is_empty());
        assert_eq!(plan.unscoped_legacy_counts[&LegacyPathStoreKindV1::Note], 1);
    }

    /// retain-quarantine appends nothing, so the pair does not move.
    #[test]
    fn retain_quarantine_appends_nothing() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "task",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![LegacyPathDispositionV1::RetainQuarantine {
                legacy_path_binding_id: binding_id("a1"),
            }],
            operator_notes: Vec::new(),
        };
        let plan = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap();
        assert!(plan.appended.is_empty());
    }

    /// Section 7.3: a disposition naming a NON-quarantined binding is
    /// inconsistent with the predecessor inventory and refuses.
    #[test]
    fn resolving_a_non_quarantined_binding_refuses() {
        let attachments = attachments_with(vec![entry("a1", 1, mapped(project('a')), "knowledge")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![LegacyPathDispositionV1::MapToProjectId {
                legacy_path_binding_id: binding_id("a1"),
                project_id: project('b'),
                relationship: LegacyPathRelationship::Root,
            }],
            operator_notes: Vec::new(),
        };
        let error = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// A disposition naming an unknown binding refuses with the same code.
    #[test]
    fn resolving_an_unknown_binding_refuses() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "knowledge",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![LegacyPathDispositionV1::RetainQuarantine {
                legacy_path_binding_id: binding_id("ff"),
            }],
            operator_notes: Vec::new(),
        };
        let error = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// A resolution captured against a different predecessor is stale, not
    /// invalid: it conforms to the section 7.2 suffix family.
    #[test]
    fn a_resolution_from_another_predecessor_is_stale() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "knowledge",
        )]);
        let error = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(hash(9)),
        )
        .unwrap_err();
        assert_eq!(error.code, ERROR_STALE_RESOLUTION);
    }

    /// The plan drops `acknowledged-delete` entirely: there is no `Deleted`
    /// status to express it, so an artifact naming one fails to DECODE with
    /// the section 7.3 code rather than decoding to an empty disposition list.
    #[test]
    fn a_delete_disposition_fails_to_decode() {
        let bytes = br#"{"version":1,"inventory_hash":"0000000000000000000000000000000000000000000000000000000000000000","legacy_path_dispositions":[{"disposition":"acknowledged_delete","legacy_path_binding_id":"lpb_11111111111111111111111111111111"}]}"#;
        let error = decode_resolution(bytes).unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// Naming one binding twice is an ambiguous resolution, not a last-wins.
    #[test]
    fn naming_one_binding_twice_refuses() {
        let attachments = attachments_with(vec![entry(
            "a1",
            1,
            LegacyPathBindingStatus::Quarantined {},
            "knowledge",
        )]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let resolution = DurableBackfillResolutionV1 {
            version: DURABLE_BACKFILL_RESOLUTION_VERSION_V1,
            inventory_hash,
            legacy_path_dispositions: vec![
                LegacyPathDispositionV1::RetainQuarantine {
                    legacy_path_binding_id: binding_id("a1"),
                },
                LegacyPathDispositionV1::ClassifyUnscopedByAppendedSupersession {
                    legacy_path_binding_id: binding_id("a1"),
                },
            ],
            operator_notes: Vec::new(),
        };
        let error = plan_backfill(&attachments, 7, &hash(1), &hash(2), &resolution).unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// An appended binding's id is content-derived, so re-planning the same
    /// conversion converges on the same row instead of appending a second one.
    #[test]
    fn appended_binding_ids_are_content_derived_and_stable() {
        let first =
            derive_appended_binding_id(&binding_id("a1"), 2, &LegacyPathBindingStatus::Unscoped {});
        let second =
            derive_appended_binding_id(&binding_id("a1"), 2, &LegacyPathBindingStatus::Unscoped {});
        assert_eq!(first, second);
        let other =
            derive_appended_binding_id(&binding_id("a1"), 3, &LegacyPathBindingStatus::Unscoped {});
        assert_ne!(first, other, "a different epoch is a different row");
    }

    /// The inventory hash is a function of the predecessor, so a report
    /// captured against one root cannot reproduce it against another. This is
    /// the cross-root guard the apply identity check rests on.
    #[test]
    fn the_inventory_hash_binds_the_predecessor() {
        let attachments = attachments_with(vec![entry("a1", 1, mapped(project('a')), "knowledge")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let here = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let elsewhere = backfill_inventory_hash(7, &hash(3), &hash(2), &effective);
        assert_ne!(here, elsewhere);
        let later = backfill_inventory_hash(8, &hash(1), &hash(2), &effective);
        assert_ne!(here, later);
    }

    /// The 14-variant owner token mapping round-trips in both directions.
    #[test]
    fn every_owner_token_round_trips() {
        for kind in [
            LegacyPathStoreKindV1::Knowledge,
            LegacyPathStoreKindV1::Gap,
            LegacyPathStoreKindV1::Thread,
            LegacyPathStoreKindV1::Note,
            LegacyPathStoreKindV1::Pin,
            LegacyPathStoreKindV1::Roadmap,
            LegacyPathStoreKindV1::Packet,
            LegacyPathStoreKindV1::Task,
            LegacyPathStoreKindV1::Proposal,
            LegacyPathStoreKindV1::SlackBinding,
            LegacyPathStoreKindV1::Whiteboard,
            LegacyPathStoreKindV1::Artifact,
            LegacyPathStoreKindV1::Provenance,
            LegacyPathStoreKindV1::TranscriptEdge,
        ] {
            assert_eq!(
                legacy_store_kind_from_token(legacy_store_token(kind)),
                Some(kind)
            );
        }
        assert_eq!(legacy_store_kind_from_token("not-an-owner"), None);
    }

    /// A ledger naming a store outside the owner set refuses rather than
    /// silently skipping the row.
    #[test]
    fn a_ledger_store_outside_the_owner_set_refuses() {
        let attachments =
            attachments_with(vec![entry("a1", 1, mapped(project('a')), "not-an-owner")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let error = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    // Note how much SMALLER these are than the PublisherBindingDispositionV1
    // versions they replace: no attachment_id, no accepted_commit, no
    // payload_hashes. Those three had to be fabricated to satisfy that type's
    // constructor even though the verification never reads them and the
    // migration marker does not carry them - which is exactly why the
    // marker-sourced path uses a purpose-built projection instead.
    fn seed_g1(
        project_id: ProjectId,
        generation_id: &str,
        pointer_sha256: Sha256ValueV1,
    ) -> BackfillPublisherDispositionV1 {
        BackfillPublisherDispositionV1::SeedG1 {
            project_id,
            expected_scope: PublishedScope::try_new("repo-alpha", "alpha").unwrap(),
            full_ref: "refs/heads/main".to_string(),
            generation_id: generation_id.to_string(),
            pointer_sha256,
        }
    }

    fn acknowledged(project_id: ProjectId) -> BackfillPublisherDispositionV1 {
        BackfillPublisherDispositionV1::NoPublishedContentAcknowledged {
            project_id,
            expected_scope: PublishedScope::try_new("repo-beta", "beta").unwrap(),
            full_ref: "refs/heads/main".to_string(),
        }
    }

    /// The generation path this verification checks must be the path the
    /// accepted-publication store actually WRITES.
    ///
    /// Pinned against `AcceptedPublicationStorePaths::generation` rather than
    /// restated, because a private re-derivation drifting from the store's own
    /// layout is exactly the defect this test exists because of: the check
    /// joined the generation id straight onto the root, matched nothing the
    /// store ever writes, and reported GenerationMissing for every SeedG1
    /// disposition on every real migrated root.
    #[test]
    fn the_generation_probe_uses_the_stores_own_path_shape() {
        let root = tempfile::tempdir().unwrap();
        let projects_path = root.path().join("projects.json");
        let paths = crate::accepted_publication_store::AcceptedPublicationStorePaths::derive(
            &projects_path,
        )
        .unwrap();
        let project_id = project('e');
        let generation_id =
            crate::accepted_publication_store::AcceptedPublicationGenerationId::parse(
                "a".repeat(64),
            )
            .unwrap();

        let canonical = paths.generation(&project_id, &generation_id);
        // The shape the verification builds, spelled out the same way.
        let probed = paths
            .generations()
            .join(project_id.as_str())
            .join(format!("{generation_id}.json"));

        assert_eq!(
            canonical, probed,
            "the verification's generation path drifted from the store's layout"
        );
    }

    /// D-040, the branch most likely to be got backwards: a
    /// NoPublishedContentAcknowledged project must have NO pointer. A pointer
    /// found there is a DEFECT, not a gap the backfill may fill.
    #[test]
    fn a_pointer_on_the_acknowledged_branch_is_a_defect() {
        let root = tempfile::tempdir().unwrap();
        let pointers = root.path().join("pointers");
        let generations = root.path().join("generations");
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(&generations).unwrap();
        let project_id = project('c');
        std::fs::write(
            pointers.join(format!("{}.json", project_id.as_str())),
            b"{}",
        )
        .unwrap();
        let rows =
            verify_publisher_dispositions(&[acknowledged(project_id)], &pointers, &generations)
                .unwrap();
        assert_eq!(
            rows[0].outcome,
            PublisherVerificationOutcomeV1::PointerUnexpectedlyPresent
        );
        assert!(rows[0].outcome.is_defect());
    }

    /// The same branch with no pointer is the PASS, not a missing-evidence
    /// failure.
    #[test]
    fn pointer_absence_on_the_acknowledged_branch_passes() {
        let root = tempfile::tempdir().unwrap();
        let pointers = root.path().join("pointers");
        let generations = root.path().join("generations");
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(&generations).unwrap();
        let rows =
            verify_publisher_dispositions(&[acknowledged(project('d'))], &pointers, &generations)
                .unwrap();
        assert_eq!(
            rows[0].outcome,
            PublisherVerificationOutcomeV1::PointerAbsentAsRequired
        );
        assert!(!rows[0].outcome.is_defect());
    }

    /// F1's core: an accepted-publication artifact that cannot be READ is never
    /// PROVEN ABSENT.
    ///
    /// Every shape below made `Path::is_file()` answer `false` while the truth
    /// was "I could not tell", and on the acknowledged branch `false` was the
    /// PASS - so a D-040 proof that a project publishes nothing was satisfied by
    /// an I/O error. Each now refuses.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_pointer_is_never_proven_absence() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let generations = root.join("generations");
        let pointers = root.join("pointers");
        std::fs::create_dir_all(&generations).unwrap();
        std::fs::create_dir_all(&pointers).unwrap();
        let project_id = project('c');
        let name = format!("{}.json", project_id.as_str());
        let probe = |pointers: &Path| {
            verify_publisher_dispositions(
                &[acknowledged(project_id.clone())],
                pointers,
                &generations,
            )
        };

        // ENOTDIR: the pointers root is not a directory at all.
        let not_a_directory = root.join("pointers-file");
        std::fs::write(&not_a_directory, b"{}").unwrap();
        assert_eq!(
            probe(&not_a_directory)
                .expect_err("an unreadable pointer root is not proof of absence")
                .code,
            ERROR_STALE_POST_IMAGE
        );

        // The pointer name exists but is a DIRECTORY: present, unreadable as a
        // pointer, and emphatically not absent.
        std::fs::create_dir(pointers.join(&name)).unwrap();
        assert_eq!(
            probe(&pointers)
                .expect_err("a non-regular pointer is not proof of absence")
                .code,
            ERROR_STALE_POST_IMAGE
        );
        std::fs::remove_dir(pointers.join(&name)).unwrap();

        // A SYMLINK: `is_file()` follows it and answers about a file outside the
        // root; the confined probe refuses to resolve it at all.
        std::fs::write(root.join("elsewhere.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(root.join("elsewhere.json"), pointers.join(&name)).unwrap();
        assert_eq!(
            probe(&pointers)
                .expect_err("a symlinked pointer is not the target's pointer")
                .code,
            ERROR_STALE_POST_IMAGE
        );
        std::fs::remove_file(pointers.join(&name)).unwrap();

        // A pointer this process may not open: the permission failure that made
        // the finding a HIGH rather than a nicety.
        std::fs::write(pointers.join(&name), b"{}").unwrap();
        std::fs::set_permissions(&pointers, std::fs::Permissions::from_mode(0o000)).unwrap();
        let denied = std::fs::read(pointers.join(&name)).is_err();
        let observed = probe(&pointers);
        // Restore first: a failed assertion must not leave an unremovable
        // directory behind for the harness to trip on.
        std::fs::set_permissions(&pointers, std::fs::Permissions::from_mode(0o755)).unwrap();
        if denied {
            assert_eq!(
                observed
                    .expect_err("a permission failure is not proof of absence")
                    .code,
                ERROR_STALE_POST_IMAGE
            );
        }
        // When the process CAN read through mode 0 (a privileged runner), the
        // three shapes above already carry the property; nothing is asserted
        // about a probe that genuinely succeeded.
    }

    /// The SeedG1 branch is confined too: a symlinked pointer is refused rather
    /// than followed, so bytes from OUTSIDE the pointers root can never satisfy
    /// a hash-consistency proof.
    #[cfg(unix)]
    #[test]
    fn a_symlinked_pointer_cannot_satisfy_the_seed_g1_proof() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let pointers = root.join("pointers");
        let generations = root.join("generations");
        let project_id = project('e');
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(generations.join(project_id.as_str())).unwrap();
        std::fs::write(
            generations.join(project_id.as_str()).join("g1.json"),
            b"generation-bytes",
        )
        .unwrap();

        // Bytes that WOULD satisfy the proof, sitting outside the root.
        let pointer_bytes = b"pointer-bytes";
        std::fs::write(root.join("outside.json"), pointer_bytes).unwrap();
        std::os::unix::fs::symlink(
            root.join("outside.json"),
            pointers.join(format!("{}.json", project_id.as_str())),
        )
        .unwrap();

        let error = verify_publisher_dispositions(
            &[seed_g1(
                project_id,
                "g1",
                Sha256ValueV1::digest(pointer_bytes),
            )],
            &pointers,
            &generations,
        )
        .expect_err("a link out of the pointers root is not the target's evidence");
        assert_eq!(error.code, ERROR_STALE_POST_IMAGE);
    }

    /// The stamp the journal carries is a function of the COMPLETE verification,
    /// so any change to the observed set changes it.
    #[test]
    fn the_publisher_verification_digest_binds_the_complete_set() {
        let root = tempfile::tempdir().unwrap();
        let pointers = root.path().join("pointers");
        let generations = root.path().join("generations");
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(&generations).unwrap();

        let before =
            verify_publisher_dispositions(&[acknowledged(project('d'))], &pointers, &generations)
                .unwrap();
        let empty = publisher_verification_digest(&[]);
        assert_ne!(
            publisher_verification_digest(&before),
            empty,
            "an observed disposition must not digest like an empty set"
        );

        // The same project, now unexpectedly publishing: same population, one
        // different outcome, and the stamp must move.
        std::fs::write(
            pointers.join(format!("{}.json", project('d').as_str())),
            b"{}",
        )
        .unwrap();
        let after =
            verify_publisher_dispositions(&[acknowledged(project('d'))], &pointers, &generations)
                .unwrap();
        assert_ne!(
            publisher_verification_digest(&before),
            publisher_verification_digest(&after)
        );
    }

    /// SeedG1 proves the opposite: pointer AND generation present, and the
    /// pointer bytes hash-consistent with the reviewed disposition.
    #[test]
    fn seed_g1_requires_a_present_hash_consistent_pointer_and_generation() {
        let root = tempfile::tempdir().unwrap();
        let pointers = root.path().join("pointers");
        let generations = root.path().join("generations");
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(&generations).unwrap();
        let project_id = project('e');
        let pointer_bytes = b"pointer-bytes";
        std::fs::write(
            pointers.join(format!("{}.json", project_id.as_str())),
            pointer_bytes,
        )
        .unwrap();
        // The store's real shape: <generations>/<project_id>/<id>.json. The
        // previous fixture created a bare `<generations>/g1` DIRECTORY, which
        // is why a verification that looked there passed its test and failed
        // against every real store.
        std::fs::create_dir_all(generations.join(project_id.as_str())).unwrap();
        std::fs::write(
            generations.join(project_id.as_str()).join("g1.json"),
            b"generation-bytes",
        )
        .unwrap();

        let consistent = verify_publisher_dispositions(
            &[seed_g1(
                project_id.clone(),
                "g1",
                Sha256ValueV1::digest(pointer_bytes),
            )],
            &pointers,
            &generations,
        )
        .unwrap();
        assert_eq!(
            consistent[0].outcome,
            PublisherVerificationOutcomeV1::PointerPresentAndConsistent
        );

        let mismatched = verify_publisher_dispositions(
            &[seed_g1(project_id.clone(), "g1", hash(9))],
            &pointers,
            &generations,
        )
        .unwrap();
        assert_eq!(
            mismatched[0].outcome,
            PublisherVerificationOutcomeV1::PointerHashMismatch
        );

        let no_generation = verify_publisher_dispositions(
            &[seed_g1(
                project_id,
                "g-absent",
                Sha256ValueV1::digest(pointer_bytes),
            )],
            &pointers,
            &generations,
        )
        .unwrap();
        assert_eq!(
            no_generation[0].outcome,
            PublisherVerificationOutcomeV1::GenerationMissing
        );
    }

    /// A SeedG1 project with no pointer at all is a defect: the migration
    /// transaction was supposed to install one and this command never seeds.
    #[test]
    fn seed_g1_without_a_pointer_is_a_defect() {
        let root = tempfile::tempdir().unwrap();
        let pointers = root.path().join("pointers");
        let generations = root.path().join("generations");
        std::fs::create_dir_all(&pointers).unwrap();
        std::fs::create_dir_all(&generations).unwrap();
        let rows = verify_publisher_dispositions(
            &[seed_g1(project('f'), "g1", hash(1))],
            &pointers,
            &generations,
        )
        .unwrap();
        assert_eq!(
            rows[0].outcome,
            PublisherVerificationOutcomeV1::PointerMissing
        );
        assert!(rows[0].outcome.is_defect());
    }

    /// The completion journal round-trips and is durable before apply returns.
    #[test]
    fn the_completion_journal_round_trips_through_its_state_dir() {
        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().canonicalize().unwrap();
        assert!(
            read_backfill_completion_journal(&state_dir)
                .unwrap()
                .is_none()
        );
        let journal = BackfillCompletionJournalV1 {
            version: BACKFILL_COMPLETION_JOURNAL_VERSION_V1,
            completed_at: "2026-07-26T00:00:00Z".to_string(),
            predecessor_catalog_epoch: 7,
            predecessor_catalog_hash: hash(1),
            predecessor_attachment_hash: hash(2),
            post_image_catalog_epoch: 7,
            stamp_counts: BTreeMap::from([(
                LegacyPathStoreKindV1::Knowledge,
                BackfillStoreStampCountsV1 {
                    mappable: 3,
                    stamped: 2,
                    already_stamped: 1,
                    converted: 0,
                    unscoped: 0,
                },
            )]),
            converted_binding_ids: BTreeSet::new(),
            identity: BackfillArtifactIdentityV1 {
                inventory_hash: hash(3),
                plan_hash: hash(4),
                report_artifact_hash: hash(5),
                resolution_artifact_hash: hash(6),
            },
            publisher_verification_digest: publisher_verification_digest(&[]),
        };
        write_backfill_completion_journal(&state_dir, &journal).unwrap();
        let read_back = read_backfill_completion_journal(&state_dir)
            .unwrap()
            .expect("journal is present after a synced write");
        assert_eq!(read_back, journal);
        assert_eq!(read_back.applied_stamp_total(), 3);
        assert_eq!(
            backfill_completion_journal_path(&state_dir)
                .file_name()
                .unwrap(),
            BACKFILL_COMPLETION_JOURNAL_FILE
        );
    }

    /// A journal worth publishing, for the durability tests below.
    fn sample_journal() -> BackfillCompletionJournalV1 {
        BackfillCompletionJournalV1 {
            version: BACKFILL_COMPLETION_JOURNAL_VERSION_V1,
            completed_at: "2026-08-05T00:00:00Z".to_string(),
            predecessor_catalog_epoch: 7,
            predecessor_catalog_hash: hash(1),
            predecessor_attachment_hash: hash(2),
            post_image_catalog_epoch: 7,
            stamp_counts: BTreeMap::new(),
            converted_binding_ids: BTreeSet::new(),
            identity: BackfillArtifactIdentityV1 {
                inventory_hash: hash(3),
                plan_hash: hash(4),
                report_artifact_hash: hash(5),
                resolution_artifact_hash: hash(6),
            },
            publisher_verification_digest: publisher_verification_digest(&[]),
        }
    }

    fn injected_publication_fault(
        at: JournalDurabilityStepV1,
    ) -> impl Fn(JournalDurabilityStepV1) -> BackfillResult<()> {
        move |step| {
            if step == at {
                Err(post_mutation_refusal(
                    ERROR_STALE_POST_IMAGE,
                    "injected journal publication fault",
                ))
            } else {
                Ok(())
            }
        }
    }

    /// A publication that fails BEFORE the replacement publishes nothing, and
    /// still reports the mutation that already happened upstream.
    #[test]
    fn a_pre_rename_publication_failure_publishes_nothing_and_stays_retryable() {
        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().canonicalize().unwrap();

        let error = publish_backfill_completion_journal(
            &state_dir,
            &sample_journal(),
            injected_publication_fault(JournalDurabilityStepV1::BeforeReplace),
        )
        .expect_err("the injected pre-replacement fault must refuse");

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired,
            "a failure past the pair transaction is never no-mutation"
        );
        assert!(
            !backfill_completion_journal_path(&state_dir).exists(),
            "a pre-replacement failure must publish nothing"
        );
        // And no residue is left behind for the next publication to trip on.
        assert_eq!(
            std::fs::read_dir(&state_dir).unwrap().count(),
            0,
            "a failed publication leaves no temporary behind"
        );
    }

    /// THE MISREPORT F3 NAMES: a failure AFTER the replacement landed, which the
    /// previous revision routed through the no-mutation constructor. The pair
    /// transaction, the stamp set, AND the journal are all on disk by then, so
    /// telling the operator nothing was mutated invites a retry of a plan whose
    /// predecessor has already moved.
    #[test]
    fn a_post_rename_publication_failure_is_not_reported_as_no_mutation() {
        let root = tempfile::tempdir().unwrap();
        let state_dir = root.path().canonicalize().unwrap();

        let error = publish_backfill_completion_journal(
            &state_dir,
            &sample_journal(),
            injected_publication_fault(JournalDurabilityStepV1::AfterReplace),
        )
        .expect_err("the injected post-replacement fault must refuse");

        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired
        );
        assert!(
            backfill_completion_journal_path(&state_dir).is_file(),
            "the replacement genuinely landed before the fault, which is what \
             makes the disposition the point"
        );
    }

    /// The parent handle the MANDATORY fsync rides on fails CLOSED.
    ///
    /// A state directory reached through a symlink cannot be opened nofollow, so
    /// the publication refuses. The previous revision opened the directory with
    /// `if let Ok(..)` and discarded the result, so this case, and a genuine
    /// fsync failure with it, passed as a complete apply.
    #[cfg(unix)]
    #[test]
    fn a_state_directory_that_cannot_be_opened_nofollow_fails_the_publication() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let real = root.join("live");
        std::fs::create_dir_all(&real).unwrap();
        let linked = root.join("linked");
        std::os::unix::fs::symlink(&real, &linked).unwrap();

        let error = write_backfill_completion_journal(&linked, &sample_journal())
            .expect_err("a state directory behind a symlink is not a publication target");

        assert_eq!(error.code, ERROR_STALE_POST_IMAGE);
        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::RetryExactPlanRequired
        );
        assert!(
            !backfill_completion_journal_path(&real).exists(),
            "nothing may be written through the link"
        );
    }

    /// A leftover at the OLD FIXED staging name neither wedges the publication
    /// nor redirects it out of the state directory.
    ///
    /// Both halves were real with a fixed `backfill-completion.json.staging`:
    /// a leftover DIRECTORY made `File::create` fail forever, and a leftover
    /// SYMLINK made it write the journal bytes to the link's target and then
    /// rename the link into place. The unique `O_EXCL` temporary makes both
    /// irrelevant.
    #[cfg(unix)]
    #[test]
    fn a_leftover_fixed_name_temporary_neither_wedges_nor_redirects_the_publication() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let state_dir = root.join("live");
        std::fs::create_dir_all(&state_dir).unwrap();
        let fixed_staging =
            backfill_completion_journal_path(&state_dir).with_extension("json.staging");

        // Half one: a leftover directory at the fixed name.
        std::fs::create_dir(&fixed_staging).unwrap();
        write_backfill_completion_journal(&state_dir, &sample_journal())
            .expect("a stale fixed-name leftover must not wedge the publication");
        std::fs::remove_dir(&fixed_staging).unwrap();
        std::fs::remove_file(backfill_completion_journal_path(&state_dir)).unwrap();

        // Half two: a leftover symlink pointing OUT of the state directory.
        let escape = root.join("escape.json");
        std::os::unix::fs::symlink(&escape, &fixed_staging).unwrap();
        write_backfill_completion_journal(&state_dir, &sample_journal())
            .expect("a stale fixed-name symlink must not redirect the publication");

        assert!(
            !escape.exists(),
            "journal bytes must never be written through a symlink out of the \
             state directory"
        );
        let published = backfill_completion_journal_path(&state_dir);
        assert!(
            !std::fs::symlink_metadata(&published)
                .unwrap()
                .file_type()
                .is_symlink(),
            "the published journal must be a regular file, not a link"
        );
        assert_eq!(
            read_backfill_completion_journal(&state_dir).unwrap(),
            Some(sample_journal())
        );
    }

    /// The report carries the resolution's hash but never its own, and the
    /// resolution never carries the report's: the artifact hash graph is
    /// acyclic (FD-4).
    #[test]
    fn the_artifact_hash_graph_is_acyclic() {
        let report = DurableBackfillReportV1 {
            version: DURABLE_BACKFILL_REPORT_VERSION_V1,
            generated_at: "2026-07-26T00:00:00Z".to_string(),
            status: DurableBackfillStatusV1::Clean,
            inventory_hash: hash(1),
            plan_hash: hash(2),
            resolution_artifact_hash: hash(3),
            predecessor_catalog_epoch: 7,
            predecessor_catalog_hash: hash(4),
            predecessor_attachment_hash: hash(5),
            ledger_rows: Vec::new(),
            classification_counts: BTreeMap::new(),
            planned_stamps: BTreeMap::new(),
            unscoped_legacy_counts: BTreeMap::new(),
            publisher_verification: Vec::new(),
            stamp_coverage: BTreeMap::new(),
            predicted_post_image_catalog_epoch: 7,
            completion_journal_relative_path: BACKFILL_COMPLETION_JOURNAL_FILE.to_string(),
        };
        let bytes = encode_report(&report).unwrap();
        let own_hash = Sha256ValueV1::digest(&bytes);
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            !text.contains(own_hash.as_str()),
            "a report must never contain its own byte hash"
        );
        let resolution = DurableBackfillResolutionV1::empty(hash(1));
        let resolution_text = String::from_utf8(encode_resolution(&resolution).unwrap()).unwrap();
        assert!(
            !resolution_text.contains(hash(3).as_str()),
            "a resolution must never contain the report's hash"
        );
        assert_eq!(
            decode_report(&encode_report(&report).unwrap()).unwrap(),
            report
        );
    }

    /// A persisted artifact carries digests, never host path literals.
    #[test]
    fn the_report_redacts_host_paths() {
        let attachments = attachments_with(vec![entry("a1", 1, mapped(project('a')), "knowledge")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let plan = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap();
        let text = serde_json::to_string(&plan.rows).unwrap();
        assert!(!text.contains("/host/checkouts/alpha"));
        assert!(text.contains(digest_path("/host/checkouts/alpha").as_str()));
    }

    /// A stamper answering by exhaustive match over the owner set. A production
    /// impl has this shape, which is why an added variant breaks the build
    /// rather than silently defaulting to uncovered.
    struct PartialStamper;

    impl LegacyRowStamperV1 for PartialStamper {
        fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
            match store_kind {
                LegacyPathStoreKindV1::Knowledge | LegacyPathStoreKindV1::Gap => {
                    LegacyRowStampCoverageV1::Covered
                }
                LegacyPathStoreKindV1::Thread
                | LegacyPathStoreKindV1::Note
                | LegacyPathStoreKindV1::Pin
                | LegacyPathStoreKindV1::Roadmap
                | LegacyPathStoreKindV1::Packet
                | LegacyPathStoreKindV1::Task
                | LegacyPathStoreKindV1::Proposal
                | LegacyPathStoreKindV1::SlackBinding
                | LegacyPathStoreKindV1::Whiteboard
                | LegacyPathStoreKindV1::Artifact
                | LegacyPathStoreKindV1::Provenance
                | LegacyPathStoreKindV1::TranscriptEdge => LegacyRowStampCoverageV1::NotImplemented,
            }
        }

        fn stamp(
            &self,
            _store_kind: LegacyPathStoreKindV1,
            _source_row_id: &str,
            _project_id: &ProjectId,
        ) -> BackfillResult<LegacyRowStampOutcomeV1> {
            Ok(LegacyRowStampOutcomeV1::Stamped)
        }
    }

    /// A stamper shaped like production for the Q-E3b question: provenance is
    /// EXEMPT, not unimplemented.
    struct ExemptProvenanceStamper;

    impl LegacyRowStamperV1 for ExemptProvenanceStamper {
        fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
            match store_kind {
                LegacyPathStoreKindV1::Provenance => LegacyRowStampCoverageV1::ExemptByConstruction,
                LegacyPathStoreKindV1::Task => LegacyRowStampCoverageV1::NotImplemented,
                _ => LegacyRowStampCoverageV1::Covered,
            }
        }

        fn stamp(
            &self,
            _store_kind: LegacyPathStoreKindV1,
            _source_row_id: &str,
            _project_id: &ProjectId,
        ) -> BackfillResult<LegacyRowStampOutcomeV1> {
            Ok(LegacyRowStampOutcomeV1::Stamped)
        }
    }

    /// Q-E3b proof 2: a legitimate predecessor plans ZERO provenance stamps, and
    /// exemption is not mistaken for a missing write-back.
    ///
    /// The distinction matters: `uncovered_planned_stores` must NOT name an
    /// exempt owner, or every backfill would refuse forever on an owner that
    /// correctly has nothing to do.
    #[test]
    fn an_exempt_owner_with_no_planned_stamps_is_clean_and_not_uncovered() {
        let attachments = attachments_with(vec![entry("a1", 1, mapped(project('a')), "knowledge")]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);
        let plan = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap();

        assert_eq!(
            plan.planned_stamps
                .get(&LegacyPathStoreKindV1::Provenance)
                .copied()
                .unwrap_or(0),
            0,
            "a legitimate predecessor cannot plan a provenance stamp"
        );
        assert!(
            uncovered_planned_stores(&plan.planned_stamps, &ExemptProvenanceStamper).is_empty(),
            "an exempt owner must not be reported as a missing write-back"
        );
        assert!(
            exempt_stores_with_planned_stamps(&plan.planned_stamps, &ExemptProvenanceStamper)
                .is_empty()
        );
        assert_eq!(
            ExemptProvenanceStamper.coverage(LegacyPathStoreKindV1::Provenance),
            LegacyRowStampCoverageV1::ExemptByConstruction,
            "exemption is its own verdict, not Covered with a no-op stamper"
        );

        // And the verdict is RECORDED, for every owner in the closed universe.
        // Keyed by planned stores this map would omit provenance entirely,
        // which reads as "not considered" rather than "considered and exempt".
        let stamp_coverage = LegacyPathStoreKindV1::all()
            .into_iter()
            .map(|kind| (kind, ExemptProvenanceStamper.coverage(kind)))
            .collect::<BTreeMap<_, _>>();
        assert_eq!(stamp_coverage.len(), 14, "every owner gets a verdict");
        assert_eq!(
            stamp_coverage.get(&LegacyPathStoreKindV1::Provenance),
            Some(&LegacyRowStampCoverageV1::ExemptByConstruction)
        );
    }

    /// Q-E3b proof 3: an injected provenance ledger binding refuses BEFORE any
    /// mutation.
    ///
    /// Planning is where the refusal lands, which is upstream of both the pair
    /// transaction and the first stamp, so an inconsistent predecessor cannot
    /// reach durable state at all. Neither stamping the row nor comparing it
    /// would be a repair: a provenance binding means the capture association
    /// itself is invalid.
    #[test]
    fn an_injected_provenance_binding_refuses_before_any_mutation() {
        let attachments = attachments_with(vec![
            entry("a1", 1, mapped(project('a')), "knowledge"),
            entry("a2", 1, mapped(project('a')), "provenance"),
        ]);
        let effective = effective_legacy_bindings(&attachments.legacy_path_bindings).unwrap();
        let inventory_hash = backfill_inventory_hash(7, &hash(1), &hash(2), &effective);

        let error = plan_backfill(
            &attachments,
            7,
            &hash(1),
            &hash(2),
            &DurableBackfillResolutionV1::empty(inventory_hash),
        )
        .unwrap_err();

        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
        assert!(
            error.message.contains("provenance"),
            "refusal must name the owner: {}",
            error.message
        );
        assert_eq!(
            error.mutation_disposition,
            ProjectCatalogMigrationMutationDispositionV1::NoDurableMutation,
            "planning refuses before anything is written"
        );
    }

    /// The exemption is CHECKED, not asserted: if some future change did plan a
    /// provenance stamp, the coverage probe turns it into a refusal instead of
    /// silently stamping nothing and reporting success.
    #[test]
    fn an_exempt_owner_with_planned_stamps_is_named_as_a_broken_premise() {
        let planned = BTreeMap::from([
            (LegacyPathStoreKindV1::Knowledge, 2),
            (LegacyPathStoreKindV1::Provenance, 1),
        ]);

        let exempt = exempt_stores_with_planned_stamps(&planned, &ExemptProvenanceStamper);

        assert_eq!(exempt, vec!["provenance"]);
        // And it is NOT laundered through the missing-write-back channel, which
        // would report the wrong cause.
        assert!(uncovered_planned_stores(&planned, &ExemptProvenanceStamper).is_empty());
    }

    /// An owner with planned stamps and no write-back is NAMED, not skipped.
    /// This is the three-of-fourteen failure the coverage probe exists to stop.
    #[test]
    fn a_planned_store_without_a_write_back_is_named() {
        let planned = BTreeMap::from([
            (LegacyPathStoreKindV1::Knowledge, 2),
            (LegacyPathStoreKindV1::Thread, 5),
        ]);
        let uncovered = uncovered_planned_stores(&planned, &PartialStamper);
        assert_eq!(uncovered, vec!["thread"]);
    }

    /// Only stores that are actually covered stay silent.
    #[test]
    fn fully_covered_planned_stores_report_nothing_uncovered() {
        let planned = BTreeMap::from([
            (LegacyPathStoreKindV1::Knowledge, 1),
            (LegacyPathStoreKindV1::Gap, 1),
        ]);
        assert!(uncovered_planned_stores(&planned, &PartialStamper).is_empty());
    }

    /// A store with no planned stamps needs no write-back: refusing on it would
    /// block a backfill that was never going to touch that owner.
    #[test]
    fn a_store_with_no_planned_stamps_is_not_required_to_be_covered() {
        let planned = BTreeMap::from([(LegacyPathStoreKindV1::Thread, 0)]);
        assert!(uncovered_planned_stores(&planned, &PartialStamper).is_empty());
    }
}

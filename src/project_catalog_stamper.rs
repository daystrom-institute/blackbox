//! The production durable-store row stamper for the project-catalog backfill.
//!
//! PLACEMENT. This implements [`LegacyRowStamperV1`], a trait declared in
//! `bbox-indexing`, but it lives in the ROOT crate on purpose. The write side
//! needs the owner crates' REAL schemas, and the root crate is the only place
//! that sees all fourteen owners at once alongside the trait; `bbox-indexing`
//! cannot, because the task owner lives root-side and would invert the
//! dependency. This is the same root-composition principle the rebuild
//! placement ruling settled, and the facade's injected `Arc<dyn
//! LegacyRowStamperV1>` seam exists precisely so the implementation can sit
//! here while the contract stays in the indexing crate.
//!
//! The owner contract has THREE faces, and all three must agree on the stable
//! row ids the backfill ledger keys on: the capture half
//! (`capture_project_catalog_owner_snapshot`, fanned out by `bbox-indexing`'s
//! inventory adapters), the write half here, and the read half here
//! ([`ProjectCatalogOwnerRowReaderV1`], which backfill verify proves the
//! durable stamps through). The reader lives beside the stamper for the same
//! placement reason and shares its owner-path type and its single coverage
//! verdict; it is a SEPARATE trait so that a verify cannot write. They are
//! separated from capture by the dependency direction, not by choice, so that
//! agreement is a maintenance obligation: change one owner's row identity and
//! you must change all three.

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_core::project_catalog_snapshot::{
    LegacySelectorMembersV1, OWNER_ROW_ABSENT, OWNER_ROW_MEMBERS_MOVED,
    OWNER_ROW_PROJECT_ID_CONFLICT, OWNER_SOURCE_MOVED, OwnerRowBatchV1, OwnerRowProjectIdV1,
    OwnerRowRequestV1, OwnerRowStampError, OwnerRowStampOutcomeV1, OwnerSnapshotLimitsV1,
    singleton_selector_members,
};
use bbox_indexing::project_catalog_backfill::{
    ERROR_RESOLUTION_INVALID, ERROR_STALE_POST_IMAGE, LegacyRowObservationV1,
    LegacyRowOwnerReaderV1, LegacyRowStampCoverageV1, LegacyRowStampOutcomeV1, LegacyRowStamperV1,
    legacy_store_token,
};
use bbox_indexing::project_catalog_inventory::LegacyPathStoreKindV1;
use bbox_indexing::project_catalog_migration::ProjectCatalogMigrationError;

/// Absolute on-disk locations of the owners this stamper can write.
///
/// Deliberately plain paths rather than the inventory adapters' authorized
/// path type, which is private to `bbox-indexing`. Safety is re-established per
/// write by [`validate_owner_path`] plus the owner crates' own nofollow reads
/// and locked atomic replaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCatalogStamperPathsV1 {
    pub knowledge_store_path: PathBuf,
    pub gap_store_path: PathBuf,
    pub thread_store_path: PathBuf,
    pub note_store_path: PathBuf,
    pub pin_store_path: PathBuf,
    pub roadmap_store_path: PathBuf,
    pub packet_root: PathBuf,
    pub proposal_root: PathBuf,
    pub slack_store_root: PathBuf,
    pub whiteboard_root: PathBuf,
    pub artifact_root: PathBuf,
    /// Root of the JSONL edge lane tree, not a single file: the transcript-edge
    /// owner is a directory of lanes and the stamper locates a row across all
    /// of them.
    pub transcript_edge_root: PathBuf,
    pub task_store_path: PathBuf,
}

/// Reject a path that is not absolute or that contains a traversal or prefix
/// component, before any owner code opens it.
fn validate_owner_path(path: &Path) -> Result<(), ProjectCatalogMigrationError> {
    if !path.is_absolute()
        || path.file_name().is_none()
        || path
            .components()
            .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
    {
        return Err(stamp_refusal("owner path is unsafe"));
    }
    Ok(())
}

/// The one production [`LegacyRowStamperV1`].
pub struct ProjectCatalogOwnerRowStamperV1 {
    paths: ProjectCatalogStamperPathsV1,
    limits: OwnerSnapshotLimitsV1,
}

impl ProjectCatalogOwnerRowStamperV1 {
    /// Validate every owner path up front so an unsafe or malformed one fails
    /// the whole backfill immediately rather than on the row that happens to
    /// touch it. Safety is re-established per write below; this is the
    /// fail-fast, not the guarantee.
    pub fn new(
        paths: ProjectCatalogStamperPathsV1,
        limits: OwnerSnapshotLimitsV1,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        for path in [
            &paths.knowledge_store_path,
            &paths.gap_store_path,
            &paths.thread_store_path,
            &paths.note_store_path,
            &paths.pin_store_path,
            &paths.roadmap_store_path,
            &paths.packet_root,
            &paths.proposal_root,
            &paths.slack_store_root,
            &paths.whiteboard_root,
            &paths.artifact_root,
            &paths.transcript_edge_root,
            &paths.task_store_path,
        ] {
            validate_owner_path(path)?;
        }
        Ok(Self { paths, limits })
    }

    /// Re-prove the path is safe, then write.
    ///
    /// Safety is re-established for EVERY stamp rather than cached from
    /// construction, and that is load-bearing rather than defensive. The
    /// inventory adapters' authorized-path type pins the target's inode
    /// identity, which is exactly right for a read-only capture ("nothing
    /// changed underneath me") and exactly wrong for a stamping pass: an
    /// atomic replace mints a new inode, so a cached proof would reject THIS
    /// stamper's own preceding write and a multi-row pass over one store would
    /// fail on its second row. Re-validating keeps what actually matters at a
    /// write - the path is absolute and traversal-free, and the owner crates
    /// open it nofollow under an exclusive lock - immediately before each
    /// mutation.
    fn stamp_owner_path(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_id: &str,
        path: &Path,
        stamp: impl FnOnce(&Path) -> std::result::Result<OwnerRowStampOutcomeV1, OwnerRowStampError>,
    ) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
        validate_owner_path(path)?;
        map_stamp_outcome(store_kind, source_row_id, stamp(path))
    }
}

/// Translate an owner-crate stamp result into the backfill's vocabulary.
///
/// Refusals split into TWO families, per adjudication Q-E4 (section 7.2).
///
/// A row that was present and unbound at preflight but is ABSENT, or bound to a
/// DIFFERENT project, by the time apply reaches the owner is current-state
/// divergence: the durable store moved under a plan that was accurate when it
/// was captured. That is staleness, and it maps to
/// `error.project_catalog_inventory_stale_post_image`, which tells the operator
/// the truthful remedy - a fresh preflight against the new predecessor, then
/// re-apply. It is emphatically NOT a claim that the resolution artifact is
/// malformed, and reporting it as one would send the operator to audit a
/// perfectly good artifact.
///
/// Everything else - an unsafe owner path, an undecodable or missing source, an
/// unsupported owner, a corrupt row - is a defect in the artifact or in the
/// configuration it names, and keeps
/// `error.project_catalog_durable_backfill_resolution_invalid`.
///
/// Both carry the owner token, the row id, and the owner's own diagnostic token
/// so the failing store and reason survive into the bounded message. Neither
/// sets a mutation disposition here: the facade applies
/// `with_backfill_stamping_disposition` to whatever this returns, so a refusal
/// after earlier stamps still reports partial mutation and stays retryable.
fn map_stamp_outcome(
    store_kind: LegacyPathStoreKindV1,
    source_row_id: &str,
    outcome: std::result::Result<OwnerRowStampOutcomeV1, OwnerRowStampError>,
) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
    match outcome {
        Ok(OwnerRowStampOutcomeV1::Stamped) => Ok(LegacyRowStampOutcomeV1::Stamped),
        Ok(OwnerRowStampOutcomeV1::AlreadyStamped) => Ok(LegacyRowStampOutcomeV1::AlreadyStamped),
        Err(error) => Err(row_stamp_refusal(store_kind, source_row_id, error.code)),
    }
}

/// A refusal about ONE named row, classified by [`map_stamp_outcome`]'s rule.
fn row_stamp_refusal(
    store_kind: LegacyPathStoreKindV1,
    source_row_id: &str,
    diagnostic: &str,
) -> ProjectCatalogMigrationError {
    let code = match diagnostic {
        // All three are current-state divergence: the row vanished, the row is
        // bound elsewhere, or the source moved between the read and the
        // replacement. Q-E4 enumerated the first two because they were the
        // cases then known; its PRINCIPLE (state divergence, not artifact
        // invalidity) governs the third identically.
        OWNER_ROW_ABSENT
        | OWNER_ROW_PROJECT_ID_CONFLICT
        | OWNER_SOURCE_MOVED
        | OWNER_ROW_MEMBERS_MOVED => ERROR_STALE_POST_IMAGE,
        _ => ERROR_RESOLUTION_INVALID,
    };
    ProjectCatalogMigrationError::no_mutation(
        code,
        format!(
            "legacy row stamp refused: store {} row {source_row_id}: {diagnostic}",
            legacy_store_token(store_kind)
        ),
    )
}

/// A refusal that is not about a particular row's current state: an unsafe
/// configured path, or an owner with no write-back at all. Always artifact or
/// configuration invalidity, never staleness.
fn stamp_refusal(detail: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(
        ERROR_RESOLUTION_INVALID,
        format!("legacy row stamp refused: {}", detail.into()),
    )
}

impl ProjectCatalogOwnerRowStamperV1 {
    /// Exhaustive by construction. The single `match` over all 14 variants is
    /// the point: a store added to `LegacyPathStoreKindV1` breaks this build
    /// until someone answers for it, which a registry of optional per-store
    /// stampers could never enforce.
    ///
    /// An associated function rather than a method because the READ side
    /// answers with it too. One owner has one verdict: an owner whose
    /// write-back landed has a read-back, and provenance is exempt on both
    /// sides for a single reason. Two independent matches could drift into
    /// claiming an owner is writable but not verifiable.
    pub(crate) fn coverage_verdict(store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        match store_kind {
            LegacyPathStoreKindV1::Knowledge
            | LegacyPathStoreKindV1::Gap
            | LegacyPathStoreKindV1::Thread
            | LegacyPathStoreKindV1::Note
            | LegacyPathStoreKindV1::Pin
            | LegacyPathStoreKindV1::Roadmap
            | LegacyPathStoreKindV1::Packet
            | LegacyPathStoreKindV1::Proposal
            | LegacyPathStoreKindV1::SlackBinding
            | LegacyPathStoreKindV1::Whiteboard
            | LegacyPathStoreKindV1::Artifact
            // Q-E1: a typed top-level project_id on the edge record plus an
            // atomic whole-file lane replacement.
            | LegacyPathStoreKindV1::TranscriptEdge
            // Q-E2: project_id on PersistedTask AND TaskInner, so a stamp
            // survives the daemon's load-then-persist projection.
            | LegacyPathStoreKindV1::Task => LegacyRowStampCoverageV1::Covered,

            // Provenance (Q-E3b): not unimplemented, EXEMPT. Its capture
            // requires a nonempty project id, derives it from the legacy
            // project record, and emits only inventory-target rows, so it never
            // produces the legacy-selector observation a ledger binding forms
            // from. There is no obligation to write, and the Q-E3 notes-ref CAS
            // transaction is withdrawn: a mismatch here would mean an invalid
            // capture association, which rewriting Git notes cannot repair.
            LegacyPathStoreKindV1::Provenance => LegacyRowStampCoverageV1::ExemptByConstruction,
        }
    }
}

impl LegacyRowStamperV1 for ProjectCatalogOwnerRowStamperV1 {
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        Self::coverage_verdict(store_kind)
    }

    fn stamp(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_id: &str,
        expected_members: &LegacySelectorMembersV1,
        project_id: &ProjectId,
    ) -> Result<LegacyRowStampOutcomeV1, ProjectCatalogMigrationError> {
        let limits = self.limits;
        let project_id = project_id.as_str();
        match store_kind {
            LegacyPathStoreKindV1::Knowledge => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.knowledge_store_path,
                |path| {
                    bbox_knowledge::knowledge::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Gap => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.gap_store_path,
                |path| {
                    bbox_gaps::gaps::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Thread => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.thread_store_path,
                |path| {
                    bbox_threads::threads::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Note => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.note_store_path,
                |path| {
                    bbox_threads::notes::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Pin => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.pin_store_path,
                |path| {
                    bbox_stores::pins::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Roadmap => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.roadmap_store_path,
                |path| {
                    bbox_stores::roadmap::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Packet => {
                self.stamp_owner_path(store_kind, source_row_id, &self.paths.packet_root, |path| {
                    bbox_packets::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                })
            }
            LegacyPathStoreKindV1::Proposal => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.proposal_root,
                |path| {
                    bbox_corpus_core::project_catalog_snapshot::stamp_legacy_proposal_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            // One store kind, two sources. Capture emits rows from both, with
            // different id shapes, so the write side tries the channel bindings
            // first and falls through to the proposal links ONLY on a
            // row-absent answer. Any other refusal is returned as-is rather
            // than being retried against the wrong source.
            LegacyPathStoreKindV1::SlackBinding => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.slack_store_root,
                |path| match bbox_slack::slack_channel_bindings::stamp_project_catalog_owner_row(
                    path,
                    source_row_id,
                    expected_members,
                    project_id,
                    limits,
                ) {
                    Err(error) if error.code == OWNER_ROW_ABSENT => {
                        bbox_slack::slack_proposal_links::stamp_project_catalog_owner_row(
                            path,
                            source_row_id,
                            expected_members,
                            project_id,
                            limits,
                        )
                    }
                    other => other,
                },
            ),
            LegacyPathStoreKindV1::Whiteboard => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.whiteboard_root,
                |path| {
                    bbox_whiteboards::whiteboards::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Artifact => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.artifact_root,
                |path| {
                    bbox_artifacts::artifacts::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::TranscriptEdge => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.transcript_edge_root,
                |path| {
                    bbox_edge_sidecar::edge_sidecar::stamp_project_catalog_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            LegacyPathStoreKindV1::Task => self.stamp_owner_path(
                store_kind,
                source_row_id,
                &self.paths.task_store_path,
                |path| {
                    bbox_corpus_core::project_catalog_snapshot::stamp_legacy_task_owner_row(
                        path,
                        source_row_id,
                        expected_members,
                        project_id,
                        limits,
                    )
                },
            ),
            // Reached only if preflight's coverage refusal were bypassed. A
            // typed refusal, never a silent success.
            LegacyPathStoreKindV1::Provenance => Err(stamp_refusal(format!(
                "{} has no durable write-back",
                legacy_store_token(store_kind)
            ))),
        }
    }
}

/// The one production [`LegacyRowOwnerReaderV1`]: the backfill verify's proof
/// that the durable owner rows really carry the ids the ledger binds them to.
///
/// It sits beside the stamper, shares its path type, and dispatches through the
/// same fourteen-arm match, because the three halves of the owner contract -
/// capture, stamp, and read - must agree on ONE row identity. Separated from
/// the stamper as a distinct trait rather than added to it so that a verify
/// cannot write: the type system, not a convention, is what stops a verifier
/// from proving its own work.
pub struct ProjectCatalogOwnerRowReaderV1 {
    paths: ProjectCatalogStamperPathsV1,
    limits: OwnerSnapshotLimitsV1,
}

impl ProjectCatalogOwnerRowReaderV1 {
    pub fn new(
        paths: ProjectCatalogStamperPathsV1,
        limits: OwnerSnapshotLimitsV1,
    ) -> Result<Self, ProjectCatalogMigrationError> {
        for path in [
            &paths.knowledge_store_path,
            &paths.gap_store_path,
            &paths.thread_store_path,
            &paths.note_store_path,
            &paths.pin_store_path,
            &paths.roadmap_store_path,
            &paths.packet_root,
            &paths.proposal_root,
            &paths.slack_store_root,
            &paths.whiteboard_root,
            &paths.artifact_root,
            &paths.transcript_edge_root,
            &paths.task_store_path,
        ] {
            validate_owner_path(path)?;
        }
        Ok(Self { paths, limits })
    }

    /// Re-prove the path is safe, then read the WHOLE requested set from it in
    /// ONE batch. Same path discipline as the write side, for the same reason:
    /// the proof belongs immediately before the access.
    fn read_owner_path(
        &self,
        store_kind: LegacyPathStoreKindV1,
        rows: &OwnerRowRequestV1,
        path: &Path,
        read: impl FnOnce(&Path) -> std::result::Result<OwnerRowBatchV1, OwnerRowStampError>,
    ) -> Result<BTreeMap<String, LegacyRowObservationV1>, ProjectCatalogMigrationError> {
        validate_owner_path(path)?;
        map_read_batch(store_kind, rows, read(path))
    }
}

/// Translate one owner's batch read into the backfill's vocabulary.
///
/// An ABSENT row - an id the owner's batch does not hold - is an observation,
/// not a refusal: verify's job is to REPORT that a mapped binding names a row
/// the owner no longer holds, and it names the store while doing so. Every
/// other failure - an undecodable source, a corrupt row, an unsafe path - fails
/// the WHOLE batch, because a verifier that could not read a store must never
/// answer "not stamped" for any row in it.
fn map_read_batch(
    store_kind: LegacyPathStoreKindV1,
    rows: &OwnerRowRequestV1,
    outcome: std::result::Result<OwnerRowBatchV1, OwnerRowStampError>,
) -> Result<BTreeMap<String, LegacyRowObservationV1>, ProjectCatalogMigrationError> {
    let batch = outcome.map_err(|error| {
        ProjectCatalogMigrationError::no_mutation(
            ERROR_STALE_POST_IMAGE,
            format!(
                "legacy row read refused: store {}: {}",
                legacy_store_token(store_kind),
                error.code
            ),
        )
    })?;
    let mut observations = BTreeMap::new();
    for source_row_id in rows.keys() {
        let observation = match batch.get(source_row_id) {
            None => LegacyRowObservationV1::Absent,
            Some(OwnerRowProjectIdV1::Unstamped) => LegacyRowObservationV1::Unstamped,
            Some(OwnerRowProjectIdV1::Stamped(project_id)) => ProjectId::parse(project_id.clone())
                .map(LegacyRowObservationV1::StampedWith)
                .map_err(|_| {
                    ProjectCatalogMigrationError::no_mutation(
                        ERROR_STALE_POST_IMAGE,
                        format!(
                            "legacy row read refused: store {} row {source_row_id}: the \
                                 row carries a malformed project id",
                            legacy_store_token(store_kind)
                        ),
                    )
                })?,
        };
        observations.insert(source_row_id.clone(), observation);
    }
    Ok(observations)
}

impl LegacyRowOwnerReaderV1 for ProjectCatalogOwnerRowReaderV1 {
    /// The SAME verdicts the stamper gives, delegated rather than restated: an
    /// owner whose write-back exists has a read-back too, and provenance is
    /// exempt on both sides for one reason.
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
        ProjectCatalogOwnerRowStamperV1::coverage_verdict(store_kind)
    }

    /// ONE batched read per owner, dispatched through the SAME fourteen-arm
    /// match the stamper writes through.
    ///
    /// Every arm hands the whole requested set to its owner's batch reader, so
    /// an owner is locked-and-decoded once, or walked once, however many rows
    /// verify asks about. Two properties ride on that, and the second is the
    /// one a slow implementation would silently break: verifying an owner stays
    /// linear in the owner's size rather than O(rows x owner size), and every
    /// row of one owner's answer comes from ONE durable snapshot, so a verdict
    /// can never combine rows observed in different states of the same store.
    fn observe(
        &self,
        store_kind: LegacyPathStoreKindV1,
        rows: &OwnerRowRequestV1,
    ) -> Result<BTreeMap<String, LegacyRowObservationV1>, ProjectCatalogMigrationError> {
        let limits = self.limits;
        match store_kind {
            LegacyPathStoreKindV1::Knowledge => {
                self.read_owner_path(store_kind, rows, &self.paths.knowledge_store_path, |path| {
                    bbox_knowledge::knowledge::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Gap => {
                self.read_owner_path(store_kind, rows, &self.paths.gap_store_path, |path| {
                    bbox_gaps::gaps::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Thread => {
                self.read_owner_path(store_kind, rows, &self.paths.thread_store_path, |path| {
                    bbox_threads::threads::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Note => {
                self.read_owner_path(store_kind, rows, &self.paths.note_store_path, |path| {
                    bbox_threads::notes::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Pin => {
                self.read_owner_path(store_kind, rows, &self.paths.pin_store_path, |path| {
                    bbox_stores::pins::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Roadmap => {
                self.read_owner_path(store_kind, rows, &self.paths.roadmap_store_path, |path| {
                    bbox_stores::roadmap::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Packet => {
                self.read_owner_path(store_kind, rows, &self.paths.packet_root, |path| {
                    bbox_packets::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::Proposal => {
                self.read_owner_path(store_kind, rows, &self.paths.proposal_root, |path| {
                    bbox_corpus_core::project_catalog_snapshot::read_legacy_proposal_owner_rows(
                        path, rows, limits,
                    )
                })
            }
            // One store kind, TWO sources. The channel bindings source answers
            // first and the proposal links source is asked only for the ids it
            // did not hold, which is the batched form of the stamper's
            // row-absent fallthrough: same order, same precedence, one capture
            // per source rather than one per row. A channel source that cannot
            // be READ still refuses outright rather than falling through, for
            // the same reason it does on the write side.
            LegacyPathStoreKindV1::SlackBinding => {
                let path = &self.paths.slack_store_root;
                validate_owner_path(path)?;
                let mut batch = bbox_slack::slack_channel_bindings::read_project_catalog_owner_rows(
                    path, rows, limits,
                );
                if let Ok(bound) = &batch {
                    let unresolved = rows
                        .iter()
                        .filter(|(source_row_id, _)| !bound.contains_key(*source_row_id))
                        .map(|(source_row_id, members)| (source_row_id.clone(), members.clone()))
                        .collect::<OwnerRowRequestV1>();
                    if !unresolved.is_empty() {
                        batch = bbox_slack::slack_proposal_links::read_project_catalog_owner_rows(
                            path,
                            &unresolved,
                            limits,
                        )
                        .map(|links| {
                            let mut merged = bound.clone();
                            merged.extend(links);
                            merged
                        });
                    }
                }
                map_read_batch(store_kind, rows, batch)
            }
            LegacyPathStoreKindV1::Whiteboard => {
                self.read_owner_path(store_kind, rows, &self.paths.whiteboard_root, |path| {
                    bbox_whiteboards::whiteboards::read_project_catalog_owner_rows(
                        path, rows, limits,
                    )
                })
            }
            LegacyPathStoreKindV1::Artifact => {
                self.read_owner_path(store_kind, rows, &self.paths.artifact_root, |path| {
                    bbox_artifacts::artifacts::read_project_catalog_owner_rows(path, rows, limits)
                })
            }
            LegacyPathStoreKindV1::TranscriptEdge => {
                self.read_owner_path(store_kind, rows, &self.paths.transcript_edge_root, |path| {
                    bbox_edge_sidecar::edge_sidecar::read_project_catalog_owner_rows(
                        path, rows, limits,
                    )
                })
            }
            LegacyPathStoreKindV1::Task => {
                self.read_owner_path(store_kind, rows, &self.paths.task_store_path, |path| {
                    bbox_corpus_core::project_catalog_snapshot::read_legacy_task_owner_rows(
                        path, rows, limits,
                    )
                })
            }
            // Unreachable behind verify's own coverage refusal, and a typed
            // refusal rather than a silent "absent" if it were reached.
            LegacyPathStoreKindV1::Provenance => Err(ProjectCatalogMigrationError::no_mutation(
                ERROR_STALE_POST_IMAGE,
                format!(
                    "{} has no durable read-back",
                    legacy_store_token(store_kind)
                ),
            )),
        }
    }
}

#[cfg(test)]
mod owner_row_stamper_dispatch {
    use super::*;
    use LegacyRowStampCoverageV1::{Covered, ExemptByConstruction};
    use bbox_corpus_core::project_catalog_snapshot::{
        OwnerSnapshotRowValueV1, owner_row_read_captures, reset_owner_row_read_captures,
    };

    /// Every path the stamper may touch is rooted in one tempdir, so a test
    /// that reaches the wrong owner writes somewhere observable instead of
    /// silently hitting real host state.
    fn stamper(root: &Path) -> ProjectCatalogOwnerRowStamperV1 {
        for directory in [
            "packets",
            "proposals",
            "slack",
            "whiteboards",
            "artifacts",
            "edges",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        ProjectCatalogOwnerRowStamperV1::new(
            ProjectCatalogStamperPathsV1 {
                knowledge_store_path: root.join("knowledge.json"),
                gap_store_path: root.join("gaps.json"),
                thread_store_path: root.join("threads.json"),
                note_store_path: root.join("notes.json"),
                pin_store_path: root.join("pins.json"),
                roadmap_store_path: root.join("roadmap.json"),
                packet_root: root.join("packets"),
                proposal_root: root.join("proposals"),
                slack_store_root: root.join("slack"),
                whiteboard_root: root.join("whiteboards"),
                artifact_root: root.join("artifacts"),
                transcript_edge_root: root.join("edges"),
                task_store_path: root.join("tasks.json"),
            },
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap()
    }

    fn write_store(path: &Path, array_field: &str, row_id: &str) {
        std::fs::write(
            path,
            format!(
                r#"{{"version": 1, "{array_field}": [{{"id": "{row_id}", "project": "/legacy/one"}}]}}
"#
            ),
        )
        .unwrap();
    }

    fn row_project_id(path: &Path, array_field: &str) -> Option<String> {
        let document: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        document[array_field][0]["project_id"]
            .as_str()
            .map(str::to_string)
    }

    fn project() -> ProjectId {
        ProjectId::parse("a1b2c3d4".to_string()).unwrap()
    }

    /// The dispatch reaches the right owner for two DIFFERENT stores, writes
    /// each one's own row, and leaves the other store alone.
    #[test]
    fn the_dispatch_stamps_rows_in_two_different_stores() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_store(&root.join("knowledge.json"), "entries", "kb1");
        write_store(&root.join("threads.json"), "threads", "th1");
        let stamper = stamper(&root);

        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::Knowledge,
                    "kb1",
                    &singleton_selector_members("kb1"),
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::Stamped
        );
        // The thread store is untouched until its own dispatch runs.
        assert_eq!(row_project_id(&root.join("threads.json"), "threads"), None);

        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::Thread,
                    "th1",
                    &singleton_selector_members("th1"),
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::Stamped
        );

        assert_eq!(
            row_project_id(&root.join("knowledge.json"), "entries").as_deref(),
            Some("a1b2c3d4")
        );
        assert_eq!(
            row_project_id(&root.join("threads.json"), "threads").as_deref(),
            Some("a1b2c3d4")
        );
    }

    /// The transcript-edge owner reaches its lane tree through the dispatch,
    /// and its row survives a re-stamp as `AlreadyStamped` with the same id.
    #[test]
    fn the_dispatch_stamps_a_transcript_edge_lane_row() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);
        std::fs::write(
            root.join("edges").join("tool.jsonl"),
            concat!(
                r#"{"source":{"type":"task","task_id":"one"},"kind":"RAN_BASH","#,
                r#""target":{"type":"task","task_id":"two"},"provenance":"explicit","#,
                r#""confidence":"exact","metadata":{"cwd":"/repo/one"}}"#,
                "\n",
            ),
        )
        .unwrap();

        // The edge owner's obligation is a SELECTOR GROUP, so its member
        // evidence comes from the capture rather than from the id: the members
        // are lane rows, and the observation id names the group, not a row.
        let observation = bbox_edge_sidecar::edge_sidecar::capture_project_catalog_owner_snapshot(
            &root.join("edges"),
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap()
        .rows[0]
            .clone();
        let row_id = observation.stable_row_id.clone();
        let OwnerSnapshotRowValueV1::LegacyProjectSelector { members, .. } = observation.value
        else {
            panic!("the transcript-edge owner emits legacy selectors");
        };

        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::TranscriptEdge,
                    &row_id,
                    &members,
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::Stamped
        );
        // The retry is idempotent AND still agrees with the evidence: stamping
        // does not move a member id, so a completed obligation refolds to the
        // same commitment.
        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::TranscriptEdge,
                    &row_id,
                    &members,
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::AlreadyStamped
        );
    }

    /// The production READER over the same owner paths, so a test can prove
    /// what a verify observes as well as what an apply writes.
    fn reader(root: &Path) -> ProjectCatalogOwnerRowReaderV1 {
        for directory in [
            "packets",
            "proposals",
            "slack",
            "whiteboards",
            "artifacts",
            "edges",
        ] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        ProjectCatalogOwnerRowReaderV1::new(
            ProjectCatalogStamperPathsV1 {
                knowledge_store_path: root.join("knowledge.json"),
                gap_store_path: root.join("gaps.json"),
                thread_store_path: root.join("threads.json"),
                note_store_path: root.join("notes.json"),
                pin_store_path: root.join("pins.json"),
                roadmap_store_path: root.join("roadmap.json"),
                packet_root: root.join("packets"),
                proposal_root: root.join("proposals"),
                slack_store_root: root.join("slack"),
                whiteboard_root: root.join("whiteboards"),
                artifact_root: root.join("artifacts"),
                transcript_edge_root: root.join("edges"),
                task_store_path: root.join("tasks.json"),
            },
            OwnerSnapshotLimitsV1::default(),
        )
        .unwrap()
    }

    /// The requested rows of a batched read, each carrying the singleton
    /// member evidence a small JSON owner records for one row.
    fn row_ids(ids: &[&str]) -> OwnerRowRequestV1 {
        ids.iter()
            .map(|id| ((*id).to_string(), singleton_selector_members(id)))
            .collect()
    }

    /// A MULTI-ROW read of a central-JSON owner captures that owner ONCE.
    ///
    /// The batched contract is not a cost preference. A reader that looped a
    /// per-row read would take the store lock, re-read and re-decode the whole
    /// file once per row - O(rows x store size) inside the stopped-service
    /// closeout window a verify runs in - and, because each cycle is its own
    /// lock-and-capture, could answer for one owner out of several different
    /// durable states of it. Neither shows up in the returned values, which is
    /// why the captures are counted here rather than argued about.
    #[test]
    fn a_multi_row_owner_read_captures_the_store_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("knowledge.json"),
            serde_json::to_vec(&serde_json::json!({
                "version": 1,
                "entries": [
                    {"id": "kb1", "project": "/legacy/one", "project_id": "a1b2c3d4"},
                    {"id": "kb2", "project": "/legacy/two"},
                    {"id": "kb3", "project": "/legacy/three", "project_id": "a1b2c3d4"},
                ],
            }))
            .unwrap(),
        )
        .unwrap();
        let reader = reader(&root);

        let requested = row_ids(&["kb1", "kb2", "kb3", "kb-gone"]);
        reset_owner_row_read_captures();
        let observed = reader
            .observe(LegacyPathStoreKindV1::Knowledge, &requested)
            .unwrap();

        assert_eq!(
            owner_row_read_captures(),
            1,
            "four requested rows must cost ONE locked capture of the owner"
        );
        // And that single capture answers for every requested row, including
        // the one the owner does not hold.
        assert_eq!(
            observed.get("kb1"),
            Some(&LegacyRowObservationV1::StampedWith(project()))
        );
        assert_eq!(
            observed.get("kb2"),
            Some(&LegacyRowObservationV1::Unstamped)
        );
        assert_eq!(
            observed.get("kb3"),
            Some(&LegacyRowObservationV1::StampedWith(project()))
        );
        assert_eq!(
            observed.get("kb-gone"),
            Some(&LegacyRowObservationV1::Absent)
        );
        assert_eq!(observed.len(), 4, "every requested row is answered");
    }

    /// The same obligation for a TREE owner, where the per-row cost is a full
    /// walk of the tree rather than one file read.
    #[test]
    fn a_multi_row_tree_owner_read_walks_the_tree_once() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let reader = reader(&root);
        for (id, stamped) in [("pk1", true), ("pk2", false), ("pk3", true)] {
            let mut document = serde_json::json!({"id": id, "project": "/legacy/one"});
            if stamped {
                document["project_id"] = serde_json::json!("a1b2c3d4");
            }
            std::fs::write(
                root.join("packets").join(format!("{id}.json")),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
        }

        let requested = row_ids(&["pk1", "pk2", "pk3"]);
        reset_owner_row_read_captures();
        let observed = reader
            .observe(LegacyPathStoreKindV1::Packet, &requested)
            .unwrap();

        assert_eq!(
            owner_row_read_captures(),
            1,
            "three requested rows must cost ONE walk of the packet tree"
        );
        assert_eq!(
            observed.get("pk1"),
            Some(&LegacyRowObservationV1::StampedWith(project()))
        );
        assert_eq!(
            observed.get("pk2"),
            Some(&LegacyRowObservationV1::Unstamped)
        );
        assert_eq!(
            observed.get("pk3"),
            Some(&LegacyRowObservationV1::StampedWith(project()))
        );
    }

    /// THE Q-E2 PROOF SET. Five conditions, all against the REAL daemon types
    /// rather than a stand-in, which is the whole reason this owner's stamper
    /// had to live root-side.
    ///
    /// Proof 1 is the load-then-persist survival that motivated putting the
    /// field on `TaskInner` and not only `PersistedTask`: the daemon's snapshot
    /// is a projection of live memory, so a persisted-only field is erased by
    /// the first cycle. The other four are backward compatibility, unknown
    /// field survival, conflict refusal, and new-write persistence.
    #[test]
    fn the_task_owner_satisfies_the_five_q_e2_conditions() {
        use crate::orchestration::TaskStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);
        let tasks_path = root.join("tasks.json");

        // A legacy record with a cwd, no project_id, and a field this binary's
        // PersistedTask has never heard of.
        std::fs::write(
            &tasks_path,
            concat!(
                r#"[{"id":"t1","provider":"glm","session_id":"s1","events":[],"#,
                r#""last_assistant_message":null,"usage":null,"cost_usd":null,"#,
                r#""num_turns":null,"stderr":"","status":"completed","started_at":1,"#,
                r#""completed_at":2,"exit_code":0,"cwd":"/repo/one","#,
                r#""future_field":{"written_by":"a newer daemon"}}]"#,
            ),
        )
        .unwrap();

        // PROOF 4: an unstamped legacy task loads at all (backward compatible).
        // `u64::MAX` ttl: the fixture's started_at is epoch-1, and a realistic
        // ttl would prune it before the proof could run.
        let store = TaskStore::load(&root, u64::MAX);
        assert!(store.get("t1").is_some(), "unstamped task must still load");

        // Stamp it through the production dispatch.
        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::Task,
                    "t1",
                    &singleton_selector_members("t1"),
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::Stamped
        );
        let after_stamp: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&tasks_path).unwrap()).unwrap();
        assert_eq!(after_stamp[0]["project_id"], "a1b2c3d4");
        // PROOF 2: the unknown field survived the backfill mutation.
        assert_eq!(
            after_stamp[0]["future_field"]["written_by"],
            "a newer daemon"
        );

        // PROOF 1: the stamp survives a load AND the next persist. This is the
        // one that fails if project_id lives only on PersistedTask.
        let reloaded = TaskStore::load(&root, u64::MAX);
        assert_eq!(
            reloaded
                .get("t1")
                .unwrap()
                .inner
                .lock()
                .project_id
                .as_deref(),
            Some("a1b2c3d4"),
            "the stamp must reach the LIVE record, not just the persisted one"
        );
        reloaded.persist(&root);
        let after_cycle: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&tasks_path).unwrap()).unwrap();
        assert_eq!(
            after_cycle[0]["project_id"], "a1b2c3d4",
            "a load-then-persist cycle must not erase the stamp"
        );

        // PROOF 3: a conflicting project id refuses, and writes nothing.
        let before_conflict = std::fs::read(&tasks_path).unwrap();
        let conflict = stamper
            .stamp(
                LegacyPathStoreKindV1::Task,
                "t1",
                &singleton_selector_members("t1"),
                &ProjectId::parse("99999999".to_string()).unwrap(),
            )
            .unwrap_err();
        assert_eq!(conflict.code, ERROR_STALE_POST_IMAGE);
        assert_eq!(std::fs::read(&tasks_path).unwrap(), before_conflict);

        // PROOF 5: a new write carrying known catalog authority persists the
        // stable id rather than dropping it.
        {
            let task = reloaded.get("t1").unwrap();
            let mut inner = task.inner.lock();
            inner.project_id = Some("a1b2c3d4".to_string());
        }
        reloaded.persist(&root);
        let after_write: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&tasks_path).unwrap()).unwrap();
        assert_eq!(after_write[0]["project_id"], "a1b2c3d4");
    }

    /// Re-running a partially completed pass completes without double-writing,
    /// which is what makes the section 3.3 torn-backfill recovery safe.
    #[test]
    fn a_second_pass_over_stamped_rows_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_store(&root.join("knowledge.json"), "entries", "kb1");
        let stamper = stamper(&root);

        stamper
            .stamp(
                LegacyPathStoreKindV1::Knowledge,
                "kb1",
                &singleton_selector_members("kb1"),
                &project(),
            )
            .unwrap();
        let after_first = std::fs::read(root.join("knowledge.json")).unwrap();

        assert_eq!(
            stamper
                .stamp(
                    LegacyPathStoreKindV1::Knowledge,
                    "kb1",
                    &singleton_selector_members("kb1"),
                    &project(),
                )
                .unwrap(),
            LegacyRowStampOutcomeV1::AlreadyStamped
        );
        assert_eq!(
            std::fs::read(root.join("knowledge.json")).unwrap(),
            after_first
        );
    }

    /// A row absent from the owner is a typed refusal, never a quiet success.
    ///
    /// It carries the STALENESS code (Q-E4): the row was in the plan, so it was
    /// there at preflight, and its disappearance is the durable store moving
    /// under an artifact that was accurate when captured.
    #[test]
    fn an_absent_row_refuses_as_staleness_through_the_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_store(&root.join("knowledge.json"), "entries", "kb1");
        let stamper = stamper(&root);

        let error = stamper
            .stamp(
                LegacyPathStoreKindV1::Knowledge,
                "absent",
                &singleton_selector_members("absent"),
                &project(),
            )
            .unwrap_err();
        assert_eq!(error.code, ERROR_STALE_POST_IMAGE);
        // The owner token, the row id, and the owner's own diagnostic all
        // survive into the bounded message.
        assert!(
            error.message.contains("knowledge")
                && error.message.contains("absent")
                && error.message.contains(OWNER_ROW_ABSENT),
            "diagnostic lost: {}",
            error.message
        );
    }

    /// A row bound to a DIFFERENT project is the other half of the staleness
    /// family, and is never overwritten.
    #[test]
    fn a_conflicting_row_refuses_as_staleness() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("knowledge.json"),
            r#"{"version": 1, "entries": [{"id": "kb1", "project": "/legacy/one", "project_id": "99999999"}]}
"#,
        )
        .unwrap();
        let stamper = stamper(&root);

        let error = stamper
            .stamp(
                LegacyPathStoreKindV1::Knowledge,
                "kb1",
                &singleton_selector_members("kb1"),
                &project(),
            )
            .unwrap_err();
        assert_eq!(error.code, ERROR_STALE_POST_IMAGE);
        assert!(
            error.message.contains(OWNER_ROW_PROJECT_ID_CONFLICT),
            "diagnostic lost: {}",
            error.message
        );
    }

    /// The third staleness diagnostic classifies with the other two.
    ///
    /// A source that moved between read and replacement is state divergence
    /// exactly as an absent or reassigned row is, so it must not land on
    /// artifact invalidity and send the operator to audit a clean artifact.
    #[test]
    fn a_moved_owner_source_classifies_as_staleness() {
        assert_eq!(
            row_stamp_refusal(
                LegacyPathStoreKindV1::TranscriptEdge,
                "row-1",
                OWNER_SOURCE_MOVED
            )
            .code,
            ERROR_STALE_POST_IMAGE
        );
    }

    /// The Q-E4 discrimination, proven on ONE store so nothing but the defect
    /// itself differs.
    ///
    /// An artifact-semantic defect (a corrupt row: `project_id` present but not
    /// a string, which the shared contract refuses rather than overwriting) and
    /// a state drift (the same store, a row that is simply gone) must NOT
    /// collapse onto the same code. They imply opposite operator actions: audit
    /// the artifact versus re-run preflight against the moved predecessor.
    #[test]
    fn artifact_invalidity_and_state_drift_discriminate_on_one_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Same store, same stamper, same requested project: only the defect
        // differs between the two calls below.
        std::fs::write(
            root.join("knowledge.json"),
            r#"{"version": 1, "entries": [{"id": "corrupt", "project": "/legacy/one", "project_id": 7}]}
"#,
        )
        .unwrap();
        let stamper = stamper(&root);

        let artifact_defect = stamper
            .stamp(
                LegacyPathStoreKindV1::Knowledge,
                "corrupt",
                &singleton_selector_members("corrupt"),
                &project(),
            )
            .unwrap_err();
        let state_drift = stamper
            .stamp(
                LegacyPathStoreKindV1::Knowledge,
                "vanished",
                &singleton_selector_members("vanished"),
                &project(),
            )
            .unwrap_err();

        assert_eq!(artifact_defect.code, ERROR_RESOLUTION_INVALID);
        assert_eq!(state_drift.code, ERROR_STALE_POST_IMAGE);
        assert_ne!(
            artifact_defect.code, state_drift.code,
            "Q-E4 requires these two to be distinguishable"
        );
    }

    /// An unsupported owner is artifact invalidity, not staleness: no amount of
    /// re-running preflight makes a store the stamper cannot write writable.
    #[test]
    fn an_unsupported_owner_refuses_as_artifact_invalidity() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);

        let error = stamper
            .stamp(
                LegacyPathStoreKindV1::Provenance,
                "any-row",
                &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members("any-row"),
                &project(),
            )
            .unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// The whole 14-variant answer, pinned owner by owner across all THREE
    /// verdicts, so adding a store cannot quietly inherit any of them and a
    /// verdict cannot change without a test saying so.
    ///
    /// The Task/Provenance split is the Q-E3b distinction: Task has work nobody
    /// has done yet, Provenance has no work that can exist.
    #[test]
    fn coverage_answers_for_every_owner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);

        for (kind, expected) in [
            (LegacyPathStoreKindV1::Knowledge, Covered),
            (LegacyPathStoreKindV1::Gap, Covered),
            (LegacyPathStoreKindV1::Thread, Covered),
            (LegacyPathStoreKindV1::Note, Covered),
            (LegacyPathStoreKindV1::Pin, Covered),
            (LegacyPathStoreKindV1::Roadmap, Covered),
            (LegacyPathStoreKindV1::Packet, Covered),
            (LegacyPathStoreKindV1::Proposal, Covered),
            (LegacyPathStoreKindV1::SlackBinding, Covered),
            (LegacyPathStoreKindV1::Whiteboard, Covered),
            (LegacyPathStoreKindV1::Artifact, Covered),
            (LegacyPathStoreKindV1::TranscriptEdge, Covered),
            (LegacyPathStoreKindV1::Task, Covered),
            (LegacyPathStoreKindV1::Provenance, ExemptByConstruction),
        ] {
            assert_eq!(
                stamper.coverage(kind),
                expected,
                "unexpected coverage for {}",
                legacy_store_token(kind)
            );
        }
    }

    /// An uncovered owner refuses at the write seam too, so bypassing the
    /// preflight coverage gate still cannot stamp nothing and call it done.
    #[test]
    fn an_uncovered_owner_refuses_at_the_write_seam() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);

        for kind in [
            LegacyPathStoreKindV1::Task,
            LegacyPathStoreKindV1::Provenance,
        ] {
            assert!(
                stamper
                    .stamp(
                        kind,
                        "any-row",
                        &bbox_corpus_core::project_catalog_snapshot::singleton_selector_members(
                            "any-row"
                        ),
                        &project(),
                    )
                    .is_err()
            );
        }
    }
}

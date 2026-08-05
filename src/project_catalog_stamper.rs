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
//! The read half (`capture_project_catalog_owner_snapshot`, fanned out by
//! `bbox-indexing`'s inventory adapters) and this write half must agree on the
//! stable row ids the backfill ledger keys on. They are separated by the
//! dependency direction, not by choice, so that agreement is a maintenance
//! obligation: change one owner's row identity and you must change both.

use std::path::{Component, Path, PathBuf};

use bbox_corpus_core::project_catalog::ProjectId;
use bbox_corpus_core::project_catalog_snapshot::{
    OWNER_ROW_ABSENT, OWNER_ROW_PROJECT_ID_CONFLICT, OwnerRowStampError, OwnerRowStampOutcomeV1,
    OwnerSnapshotLimitsV1,
};
use bbox_indexing::project_catalog_backfill::{
    ERROR_RESOLUTION_INVALID, ERROR_STALE_POST_IMAGE, LegacyRowStampCoverageV1,
    LegacyRowStampOutcomeV1, LegacyRowStamperV1, legacy_store_token,
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
        OWNER_ROW_ABSENT | OWNER_ROW_PROJECT_ID_CONFLICT => ERROR_STALE_POST_IMAGE,
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

impl LegacyRowStamperV1 for ProjectCatalogOwnerRowStamperV1 {
    /// Exhaustive by construction. The single `match` over all 14 variants is
    /// the point: a store added to `LegacyPathStoreKindV1` breaks this build
    /// until someone answers for it, which a registry of optional per-store
    /// stampers could never enforce.
    fn coverage(&self, store_kind: LegacyPathStoreKindV1) -> LegacyRowStampCoverageV1 {
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
            | LegacyPathStoreKindV1::Artifact => LegacyRowStampCoverageV1::Covered,
            // The three owners whose rows are not JSON store rows. Each needs a
            // schema or mechanism decision that this phase does not get to make
            // unilaterally, so each is NAMED as uncovered and refuses the
            // report before any mutation rather than silently stamping nothing:
            //
            // - Task: the persisted task record has no project id field at all,
            //   only an orchestration cwd, and it is owned by the root crate
            //   that depends on THIS one.
            // - Provenance: rows are git notes documents, so writing means
            //   writing git notes - a different transaction model entirely.
            // - TranscriptEdge: rows are lines in append-only JSONL, and the
            //   edge record carries no project id field, only a free-form
            //   metadata map.
            LegacyPathStoreKindV1::Task
            | LegacyPathStoreKindV1::Provenance
            | LegacyPathStoreKindV1::TranscriptEdge => LegacyRowStampCoverageV1::NotImplemented,
        }
    }

    fn stamp(
        &self,
        store_kind: LegacyPathStoreKindV1,
        source_row_id: &str,
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
                    project_id,
                    limits,
                ) {
                    Err(error) if error.code == OWNER_ROW_ABSENT => {
                        bbox_slack::slack_proposal_links::stamp_project_catalog_owner_row(
                            path,
                            source_row_id,
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
                        project_id,
                        limits,
                    )
                },
            ),
            // Reached only if preflight's coverage refusal were bypassed. A
            // typed refusal, never a silent success.
            LegacyPathStoreKindV1::Task
            | LegacyPathStoreKindV1::Provenance
            | LegacyPathStoreKindV1::TranscriptEdge => Err(stamp_refusal(format!(
                "{} has no durable write-back",
                legacy_store_token(store_kind)
            ))),
        }
    }
}

#[cfg(test)]
mod owner_row_stamper_dispatch {
    use super::*;

    /// Every path the stamper may touch is rooted in one tempdir, so a test
    /// that reaches the wrong owner writes somewhere observable instead of
    /// silently hitting real host state.
    fn stamper(root: &Path) -> ProjectCatalogOwnerRowStamperV1 {
        for directory in ["packets", "proposals", "slack", "whiteboards", "artifacts"] {
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
                .stamp(LegacyPathStoreKindV1::Knowledge, "kb1", &project())
                .unwrap(),
            LegacyRowStampOutcomeV1::Stamped
        );
        // The thread store is untouched until its own dispatch runs.
        assert_eq!(row_project_id(&root.join("threads.json"), "threads"), None);

        assert_eq!(
            stamper
                .stamp(LegacyPathStoreKindV1::Thread, "th1", &project())
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

    /// Re-running a partially completed pass completes without double-writing,
    /// which is what makes the section 3.3 torn-backfill recovery safe.
    #[test]
    fn a_second_pass_over_stamped_rows_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        write_store(&root.join("knowledge.json"), "entries", "kb1");
        let stamper = stamper(&root);

        stamper
            .stamp(LegacyPathStoreKindV1::Knowledge, "kb1", &project())
            .unwrap();
        let after_first = std::fs::read(root.join("knowledge.json")).unwrap();

        assert_eq!(
            stamper
                .stamp(LegacyPathStoreKindV1::Knowledge, "kb1", &project())
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
            .stamp(LegacyPathStoreKindV1::Knowledge, "absent", &project())
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
            .stamp(LegacyPathStoreKindV1::Knowledge, "kb1", &project())
            .unwrap_err();
        assert_eq!(error.code, ERROR_STALE_POST_IMAGE);
        assert!(
            error.message.contains(OWNER_ROW_PROJECT_ID_CONFLICT),
            "diagnostic lost: {}",
            error.message
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
            .stamp(LegacyPathStoreKindV1::Knowledge, "corrupt", &project())
            .unwrap_err();
        let state_drift = stamper
            .stamp(LegacyPathStoreKindV1::Knowledge, "vanished", &project())
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
            .stamp(LegacyPathStoreKindV1::Task, "any-row", &project())
            .unwrap_err();
        assert_eq!(error.code, ERROR_RESOLUTION_INVALID);
    }

    /// The three owners without a durable write-back are NAMED as uncovered,
    /// and the other eleven are covered. Pinning the whole 14-variant answer
    /// means adding a store cannot quietly inherit either verdict.
    #[test]
    fn coverage_answers_for_every_owner() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let stamper = stamper(&root);

        let uncovered = [
            LegacyPathStoreKindV1::Task,
            LegacyPathStoreKindV1::Provenance,
            LegacyPathStoreKindV1::TranscriptEdge,
        ];
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
            let expected = if uncovered.contains(&kind) {
                LegacyRowStampCoverageV1::NotImplemented
            } else {
                LegacyRowStampCoverageV1::Covered
            };
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
            LegacyPathStoreKindV1::TranscriptEdge,
        ] {
            assert!(stamper.stamp(kind, "any-row", &project()).is_err());
        }
    }
}

//! Read-only planning for the Phase 6 path-free rebuild
//! (`durable-project-catalog-phase6-impl.md` section 3.4, P6-B task 5).
//!
//! This module is the PREFLIGHT half of the rebuild. Everything here scans,
//! proves, and predicts; nothing here writes. It never invokes the
//! replacement guard, never writes a prepared manifest, never creates a
//! generation, and never opens the destructive replacement path. The
//! executable half lives in the root crate's `project_catalog_rebuild_admin`,
//! which composes this planning output with the shared replacement driver
//! (adjudication Q-D).
//!
//! The load-bearing property is that preflight PREDICTS WHAT APPLY WILL
//! DECIDE. The proof mode computed here uses exactly the derivation
//! `history_materializer::select_proof_mode` uses at apply: same recorded
//! fingerprint from the persisted inventory asset, same recomputation over
//! the same three roots. If the two derivations drifted apart, a preflight
//! could report Equality and hand the operator a report authorizing a cut
//! whose apply then lands in Drift, which D-036 forbids for a cut-authorizing
//! rebuild. Reviewing a prediction that the executable path does not honour
//! is worse than not predicting at all.

use bbox_corpus_core::project_catalog::{CatalogOriginV2, ProjectCatalogTransactionId};
use bbox_corpus_index::index::history_generations::{
    HistoryProofModeV1, HistoryScanLimitsV1, scan_commit_documents,
};
use serde::Serialize;

use crate::project_catalog_inventory::Sha256ValueV1;
use crate::project_catalog_migration::{
    ProjectCatalogMigrationError, ProjectCatalogMigrationResolvedLayoutV1,
    load_legacy_commit_namespace_inventory_asset,
    recompute_legacy_commit_namespace_source_fingerprint,
};
use crate::project_catalog_rebuild::ERROR_REBUILD_PROOF_MODE;
use crate::project_catalog_store::ProjectCatalogStore;

type PlanningResult<T> = Result<T, ProjectCatalogMigrationError>;

/// Refusals raised while READING. Every one of them is `no_mutation` by
/// construction: this module has nothing to roll back.
fn refuse(code: &'static str, message: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(code, message)
}

/// The predecessor binding is missing or unusable.
///
/// Reuses the migration facade's staleness family rather than minting a code:
/// section 7.2 makes staleness a suffixed family, and a rebuild whose
/// predecessor backfill cannot be established is exactly a stale-predecessor
/// condition.
pub const ERROR_REBUILD_STALE_PREDECESSOR: &str =
    "error.project_catalog_inventory_stale_post_image";

/// What one legacy commit namespace contributes to the rebuild.
///
/// Counts and the commitment come from the READ-ONLY scan of the outgoing
/// index, which is the same scan the replacement guard performs. The
/// commitment travels because a count alone cannot distinguish "the same 400
/// documents" from "400 different documents".
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebuildNamespaceDispositionV1 {
    pub namespace: String,
    pub commit_document_count: u64,
    pub commit_document_commitment_sha256: String,
    pub truncated_message_count: u64,
}

/// The Finding-2 source binding carried by the rebuild preflight report.
///
/// Three components, exactly as P6-B task 5 names them: the source index
/// fingerprints, the namespace disposition set, and the proof mode.
///
/// All three reach VERIFY as durable state rather than as an artifact the
/// operator hands back: the committed `RepoHistoryRebuildManifestV1` records
/// the proof mode and both fingerprints, and names every generation in every
/// bucket, and the landed `ProjectCatalogPathFreeRebuildFacadeV1::verify`
/// already revalidates them. Section 3.1 is explicit that verify takes no
/// artifacts, so binding it to a required artifact path would contradict the
/// mode's definition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebuildSourceBindingV1 {
    /// The fingerprint the migration persisted in its inventory asset.
    /// `None` for a catalog that is not `MigratedV1`.
    pub recorded_source_index_fingerprint: Option<Sha256ValueV1>,
    /// The fingerprint recomputed over the index this rebuild will consume.
    /// `None` when no comparable fingerprint could be recomputed.
    pub observed_source_index_fingerprint: Option<Sha256ValueV1>,
    /// `Equality` only when both fingerprints are present and equal.
    pub proof_mode: HistoryProofModeV1,
    /// Every legacy commit namespace the rebuild will rematerialize, sorted
    /// by namespace (the scan returns a `BTreeMap`, so order is stable).
    pub namespace_dispositions: Vec<RebuildNamespaceDispositionV1>,
    /// The schema the outgoing index carries, recorded so a reviewer can see
    /// which schema the rebuild is replacing.
    pub source_schema_version: Option<String>,
}

impl RebuildSourceBindingV1 {
    /// Total commit documents the rebuild must rematerialize.
    pub fn commit_document_total(&self) -> u64 {
        self.namespace_dispositions
            .iter()
            .map(|row| row.commit_document_count)
            .sum()
    }
}

/// The predecessor this rebuild is chained to (section 3.3).
///
/// The backfill completion journal is the chain link that makes the four-hash
/// sequence contiguous whether or not the backfill advanced the catalog
/// epoch. When the backfill mutated nothing, the post-image epoch EQUALS the
/// predecessor epoch, so an epoch comparison alone cannot tell "the backfill
/// ran and changed nothing" from "the backfill never ran". The journal can,
/// which is why it is a required predecessor rather than an optimization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RebuildPredecessorBindingV1 {
    pub backfill_post_image_catalog_epoch: u64,
    pub backfill_inventory_hash: Sha256ValueV1,
    pub backfill_plan_hash: Sha256ValueV1,
    pub applied_stamp_total: u64,
    /// The catalog epoch observed NOW, which must still equal the journal's
    /// post-image epoch or the predecessor has moved underneath this plan.
    pub observed_catalog_epoch: u64,
}

/// The complete read-only rebuild plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathFreeRebuildPlanV1 {
    pub predecessor: RebuildPredecessorBindingV1,
    pub source_binding: RebuildSourceBindingV1,
    pub catalog_transaction_id: Option<ProjectCatalogTransactionId>,
}

/// Read the predecessor backfill binding and prove it still describes the
/// target (section 3.3, FD-10).
///
/// Refuses rather than replanning. A predecessor that has moved is a
/// diagnosis condition: section 6.2 says leave the service stopped, run a
/// fresh preflight, and review the new artifacts. There is no loop here and
/// no numeric cap, deliberately.
pub fn read_predecessor_binding(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    observed_catalog_epoch: u64,
) -> PlanningResult<RebuildPredecessorBindingV1> {
    let state_dir = layout.state_dir_for_backfill();
    let journal = crate::project_catalog_backfill::read_backfill_completion_journal(state_dir)?
        .ok_or_else(|| {
            refuse(
                ERROR_REBUILD_STALE_PREDECESSOR,
                "no backfill completion journal is present: the path-free rebuild is \
                 sequenced after the durable backfill (section 6.1), and its absence \
                 means the predecessor either never ran or did not complete",
            )
        })?;
    if journal.post_image_catalog_epoch != observed_catalog_epoch {
        return Err(refuse(
            ERROR_REBUILD_STALE_PREDECESSOR,
            format!(
                "the backfill completion journal records post-image catalog epoch {} but \
                 the target is now at epoch {}: the predecessor moved, so this plan \
                 blocks for diagnosis rather than replanning",
                journal.post_image_catalog_epoch, observed_catalog_epoch
            ),
        ));
    }
    Ok(RebuildPredecessorBindingV1 {
        backfill_post_image_catalog_epoch: journal.post_image_catalog_epoch,
        backfill_inventory_hash: journal.identity.inventory_hash.clone(),
        backfill_plan_hash: journal.identity.plan_hash.clone(),
        applied_stamp_total: journal.applied_stamp_total(),
        observed_catalog_epoch,
    })
}

/// Compute the Finding-2 source binding, READ-ONLY.
///
/// The proof-mode derivation deliberately mirrors
/// `history_materializer::select_proof_mode` step for step, including its
/// `state_dir/git_meta` cursor root and its use of the request's RESOLVED
/// vector root rather than a derived one (R33F1). A preflight that proved
/// Equality against a different vector store than the runtime writes could
/// never be honoured at apply.
pub fn plan_source_binding(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    origin: &CatalogOriginV2,
    scan_limits: HistoryScanLimitsV1,
) -> PlanningResult<RebuildSourceBindingV1> {
    let index_path = layout.index_root_for_rebuild();
    let recorded = match origin {
        CatalogOriginV2::FreshV2 {} => None,
        CatalogOriginV2::MigratedV1 { transaction_id } => {
            let asset = load_legacy_commit_namespace_inventory_asset(
                layout.projects_path(),
                transaction_id,
            )?
            .ok_or_else(|| {
                refuse(
                    ERROR_REBUILD_PROOF_MODE,
                    format!(
                        "migrated catalog {transaction_id} has no persisted legacy \
                         commit-namespace inventory asset, so Equality cannot be proved \
                         and legacy history cannot be rematerialized unproved"
                    ),
                )
            })?;
            Some(asset.source_index_fingerprint)
        }
    };
    // Same three roots `select_proof_mode` uses at apply: the outgoing index,
    // the `state_dir/git_meta` cursor root, and the layout's RESOLVED vector
    // root rather than one derived here (R33F1).
    let observed = recompute_legacy_commit_namespace_source_fingerprint(
        index_path,
        layout.git_meta_root_for_rebuild(),
        layout.vector_root_for_rebuild(),
    );
    let proof_mode = match (recorded.as_ref(), observed.as_ref()) {
        (Some(recorded), Some(observed)) if recorded == observed => HistoryProofModeV1::Equality,
        _ => HistoryProofModeV1::Drift,
    };

    let scan = scan_commit_documents(index_path, scan_limits)
        .map_err(|error| refuse(ERROR_REBUILD_PROOF_MODE, error.to_string()))?;
    let (namespace_dispositions, source_schema_version) = match scan {
        Some(scan) => (
            scan.namespaces
                .into_iter()
                .map(|(namespace, capture)| RebuildNamespaceDispositionV1 {
                    namespace,
                    commit_document_count: capture.commit_documents.len() as u64,
                    commit_document_commitment_sha256: capture.commit_document_commitment_sha256,
                    truncated_message_count: capture.truncated_message_count,
                })
                .collect(),
            Some(scan.schema_version),
        ),
        // NOTHING TO CARRY, in the narrow sense the replacement guard uses:
        // an absent index path, or a directory holding no tantivy index at
        // all. This is NOT "the marker is missing".
        None => (Vec::new(), None),
    };

    Ok(RebuildSourceBindingV1 {
        recorded_source_index_fingerprint: recorded,
        observed_source_index_fingerprint: observed,
        proof_mode,
        namespace_dispositions,
        source_schema_version,
    })
}

/// D-036: Equality is MANDATORY for the Phase 6 offline rebuild.
///
/// Drift proves only non-loss, which is insufficient to authorize a cut. The
/// service is stopped across the whole sequence (section 6.1), so the index
/// cannot legitimately drift between backfill apply and rebuild preflight; a
/// Drift outcome during the cut therefore indicates an inconsistent capture
/// and blocks for diagnosis rather than proceeding on the weaker proof.
pub fn require_equality_proof(binding: &RebuildSourceBindingV1) -> PlanningResult<()> {
    if binding.proof_mode == HistoryProofModeV1::Equality {
        return Ok(());
    }
    Err(refuse(
        ERROR_REBUILD_PROOF_MODE,
        format!(
            "the rebuild source binding proves {:?}, not Equality: recorded fingerprint {}, \
             observed {}. A cut-authorizing rebuild requires Equality (D-036); Drift proves \
             only non-loss.",
            binding.proof_mode,
            binding
                .recorded_source_index_fingerprint
                .as_ref()
                .map(|value| value.as_str())
                .unwrap_or("absent"),
            binding
                .observed_source_index_fingerprint
                .as_ref()
                .map(|value| value.as_str())
                .unwrap_or("absent"),
        ),
    ))
}

/// The complete read-only preflight plan for one explicit target.
///
/// Opens the catalog store READ-ONLY to read its epoch and origin, chains the
/// predecessor backfill journal, computes the source binding, and enforces
/// D-036. It writes nothing.
pub fn plan_path_free_rebuild(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    scan_limits: HistoryScanLimitsV1,
) -> PlanningResult<PathFreeRebuildPlanV1> {
    let store = ProjectCatalogStore::open_existing(layout.projects_path())
        .map_err(|error| refuse(ERROR_REBUILD_STALE_PREDECESSOR, error.to_string()))?;
    let snapshot = store
        .snapshot()
        .map_err(|error| refuse(ERROR_REBUILD_STALE_PREDECESSOR, error.to_string()))?;
    let observed_catalog_epoch = snapshot.epoch();
    let origin = snapshot.catalog().origin.clone();

    let predecessor = read_predecessor_binding(layout, observed_catalog_epoch)?;
    let source_binding = plan_source_binding(layout, &origin, scan_limits)?;
    require_equality_proof(&source_binding)?;

    Ok(PathFreeRebuildPlanV1 {
        predecessor,
        source_binding,
        catalog_transaction_id: match origin {
            CatalogOriginV2::MigratedV1 { transaction_id } => Some(transaction_id),
            CatalogOriginV2::FreshV2 {} => None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    use bbox_config::config::{self, Config};
    use tempfile::tempdir;

    use crate::project_catalog_backfill::{
        BACKFILL_COMPLETION_JOURNAL_VERSION_V1, BackfillArtifactIdentityV1,
        BackfillCompletionJournalV1, backfill_completion_journal_path,
    };
    use crate::project_catalog_migration::ProjectCatalogMigrationLayoutOverridesV1;

    fn test_config(root: &Path) -> Config {
        let _guard = bbox_util::util::test_env_lock();
        let config_path = root.join("config.toml");
        fs::write(
            &config_path,
            // `vectors_dir` is explicit: the vector root defaults to the
            // PLATFORM state directory (R33F1), and a fixture that omitted it
            // would fingerprint the host's real vector store.
            format!(
                "[paths]\nstate_dir = {:?}\nvectors_dir = {:?}\n",
                root.join("live"),
                root.join("live").join("vectors")
            ),
        )
        .unwrap();
        config::load_with(config::LoadOptions {
            config_path: Some(config_path),
            ..Default::default()
        })
        .unwrap()
    }

    fn test_layout(root: &Path) -> ProjectCatalogMigrationResolvedLayoutV1 {
        let config = test_config(root);
        ProjectCatalogMigrationResolvedLayoutV1::from_config(
            &config,
            ProjectCatalogMigrationLayoutOverridesV1 {
                state_dir: Some(root.join("live")),
                projects_path: Some(root.join("live").join("projects.json")),
            },
        )
        .unwrap()
    }

    fn hash(seed: u8) -> Sha256ValueV1 {
        Sha256ValueV1::parse(format!("{seed:02x}").repeat(32)).unwrap()
    }

    fn binding(
        recorded: Option<Sha256ValueV1>,
        observed: Option<Sha256ValueV1>,
    ) -> RebuildSourceBindingV1 {
        let proof_mode = match (recorded.as_ref(), observed.as_ref()) {
            (Some(left), Some(right)) if left == right => HistoryProofModeV1::Equality,
            _ => HistoryProofModeV1::Drift,
        };
        RebuildSourceBindingV1 {
            recorded_source_index_fingerprint: recorded,
            observed_source_index_fingerprint: observed,
            proof_mode,
            namespace_dispositions: Vec::new(),
            source_schema_version: None,
        }
    }

    fn write_journal(layout: &ProjectCatalogMigrationResolvedLayoutV1, post_image_epoch: u64) {
        let state_dir = layout.state_dir_for_backfill();
        fs::create_dir_all(state_dir).unwrap();
        let journal = BackfillCompletionJournalV1 {
            version: BACKFILL_COMPLETION_JOURNAL_VERSION_V1,
            completed_at: "2026-08-04T00:00:00Z".to_string(),
            predecessor_catalog_epoch: post_image_epoch,
            predecessor_catalog_hash: hash(0x11),
            predecessor_attachment_hash: hash(0x22),
            post_image_catalog_epoch: post_image_epoch,
            stamp_counts: Default::default(),
            identity: BackfillArtifactIdentityV1 {
                inventory_hash: hash(0x33),
                plan_hash: hash(0x44),
                report_artifact_hash: hash(0x55),
                resolution_artifact_hash: hash(0x66),
            },
        };
        fs::write(
            backfill_completion_journal_path(state_dir),
            serde_json::to_vec(&journal).unwrap(),
        )
        .unwrap();
    }

    /// D-036: Equality is MANDATORY. Drift proves only non-loss, which cannot
    /// authorize a cut, so the rebuild refuses it rather than proceeding on
    /// the weaker proof.
    #[test]
    fn drift_is_refused_and_equality_is_admitted() {
        let equal = binding(Some(hash(0xaa)), Some(hash(0xaa)));
        assert_eq!(equal.proof_mode, HistoryProofModeV1::Equality);
        require_equality_proof(&equal).expect("matching fingerprints prove Equality");

        // Every way of failing to prove Equality must refuse with the same
        // code: differing fingerprints, an unrecomputable observation, and a
        // catalog that recorded none.
        for (recorded, observed) in [
            (Some(hash(0xaa)), Some(hash(0xbb))),
            (Some(hash(0xaa)), None),
            (None, Some(hash(0xaa))),
            (None, None),
        ] {
            let refused = require_equality_proof(&binding(recorded, observed))
                .expect_err("only matching fingerprints may prove Equality");
            assert_eq!(refused.code, ERROR_REBUILD_PROOF_MODE);
        }
    }

    /// Section 6.1 sequences the rebuild AFTER the durable backfill. An
    /// absent completion journal means the predecessor never ran or did not
    /// complete, and the rebuild must not invent one.
    #[test]
    fn an_absent_backfill_journal_refuses_the_predecessor_binding() {
        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let layout = test_layout(&root);
        let refused = read_predecessor_binding(&layout, 7)
            .expect_err("the rebuild is sequenced after the backfill");
        assert_eq!(refused.code, ERROR_REBUILD_STALE_PREDECESSOR);
    }

    /// FD-10: apply never replans, and neither does planning. A predecessor
    /// that moved blocks for diagnosis.
    #[test]
    fn a_moved_predecessor_epoch_blocks_rather_than_replanning() {
        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let layout = test_layout(&root);
        write_journal(&layout, 7);

        let bound = read_predecessor_binding(&layout, 7).expect("the epoch still matches");
        assert_eq!(bound.backfill_post_image_catalog_epoch, 7);
        assert_eq!(bound.observed_catalog_epoch, 7);
        assert_eq!(bound.backfill_inventory_hash, hash(0x33));

        let refused = read_predecessor_binding(&layout, 8)
            .expect_err("an advanced epoch means the predecessor moved");
        assert_eq!(refused.code, ERROR_REBUILD_STALE_PREDECESSOR);
    }

    /// A zero-epoch backfill is the ORDINARY case, not a defect: section 3.3
    /// says the post-image epoch equals the predecessor when no quarantine
    /// conversion lands. The journal, not an epoch delta, is what proves the
    /// backfill ran, so a plan that keyed on movement would refuse every
    /// clean backfill.
    #[test]
    fn a_backfill_that_moved_no_epoch_still_binds() {
        let directory = tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let layout = test_layout(&root);
        write_journal(&layout, 0);
        let bound = read_predecessor_binding(&layout, 0)
            .expect("an unchanged epoch is the ordinary clean-backfill case");
        assert_eq!(bound.backfill_post_image_catalog_epoch, 0);
        assert_eq!(bound.applied_stamp_total, 0);
    }
}

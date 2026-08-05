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

use crate::index::history_materializer::{HistoryMaterializerRequestV1, prove_source_index};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::project_catalog_inventory::{OperatorResolutionNoteV1, Sha256ValueV1};
use crate::project_catalog_migration::{
    ProjectCatalogMigrationError, ProjectCatalogMigrationResolvedLayoutV1,
    load_legacy_commit_namespace_inventory_asset,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildPredecessorBindingV1 {
    pub backfill_post_image_catalog_epoch: u64,
    pub backfill_inventory_hash: Sha256ValueV1,
    pub backfill_plan_hash: Sha256ValueV1,
    pub applied_stamp_total: u64,
    /// The catalog epoch observed NOW, which must still equal the journal's
    /// post-image epoch or the predecessor has moved underneath this plan.
    pub observed_catalog_epoch: u64,
}

pub const PATH_FREE_REBUILD_REPORT_VERSION_V1: u32 = 1;
pub const PATH_FREE_REBUILD_RESOLUTION_VERSION_V1: u32 = 1;

/// Whether the planned rebuild is executable.
///
/// Mirrors the backfill's status rather than inventing a vocabulary: a
/// reviewer reading a rebuild report should not have to learn a second set of
/// words for the same idea (D-028).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PathFreeRebuildStatusV1 {
    Clean,
    Refused,
}

/// The four-hash identity the cut chain is bound by (FD-3, FD-4).
///
/// Lives in receipts and durable records, NEVER inside the report: no
/// artifact contains its own byte hash and no two artifacts contain each
/// other's, so the artifact hash graph stays acyclic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildArtifactIdentityV1 {
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub report_artifact_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
}

/// The reviewed rebuild report (section 3.4, FD-3 artifact vocabulary).
///
/// Carries the inventory hash, the plan hash, and the RESOLUTION artifact
/// hash. It never carries its own byte hash, and the resolution never carries
/// the report's, which is what keeps the graph acyclic (FD-4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathFreeRebuildReportV1 {
    pub version: u32,
    pub generated_at: String,
    pub status: PathFreeRebuildStatusV1,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
    pub resolution_artifact_hash: Sha256ValueV1,
    /// The predecessor backfill this rebuild is chained to (section 3.3).
    pub predecessor: RebuildPredecessorBindingV1,
    /// The Finding-2 binding: fingerprints, namespace set, proof mode.
    pub source_binding: RebuildSourceBindingV1,
    /// Total commit documents the rebuild must rematerialize, carried
    /// explicitly so a reviewer sees the magnitude without summing rows.
    pub planned_commit_document_total: u64,
}

/// The rebuild resolution.
///
/// A path-free rebuild is DETERMINISTIC: the generations it rematerializes
/// come from immutable history, and the operator chooses nothing. So the
/// canonical empty resolution is the normal case, exactly as FD-3 describes
/// for an operation with nothing to resolve. It exists at all because the
/// four-hash identity needs a resolution artifact to bind, and because a
/// reviewer may need to attach a bounded note.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathFreeRebuildResolutionV1 {
    pub version: u32,
    pub inventory_hash: Sha256ValueV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub operator_notes: Vec<OperatorResolutionNoteV1>,
}

impl PathFreeRebuildResolutionV1 {
    /// The canonical empty resolution (FD-3).
    pub fn empty(inventory_hash: Sha256ValueV1) -> Self {
        Self {
            version: PATH_FREE_REBUILD_RESOLUTION_VERSION_V1,
            inventory_hash,
            operator_notes: Vec::new(),
        }
    }
}

fn field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_be_bytes());
    hasher.update(bytes);
}

fn optional_hash_field(hasher: &mut Sha256, value: Option<&Sha256ValueV1>) {
    match value {
        // The presence marker is hashed separately from the value so an
        // absent fingerprint can never collide with a present one whose bytes
        // happen to be empty.
        Some(value) => {
            field(hasher, b"present");
            field(hasher, value.as_str().as_bytes());
        }
        None => field(hasher, b"absent"),
    }
}

/// Canonical hash over the PREDECESSOR state this plan was captured from.
///
/// Folds the backfill journal binding, because that journal is what proves
/// the predecessor ran at all when a clean backfill leaves the epoch
/// unmoved (section 3.3).
pub fn rebuild_inventory_hash(predecessor: &RebuildPredecessorBindingV1) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    field(
        &mut hasher,
        b"blackbox.project-catalog.rebuild-inventory.v1",
    );
    hasher.update(predecessor.backfill_post_image_catalog_epoch.to_be_bytes());
    hasher.update(predecessor.observed_catalog_epoch.to_be_bytes());
    field(
        &mut hasher,
        predecessor.backfill_inventory_hash.as_str().as_bytes(),
    );
    field(
        &mut hasher,
        predecessor.backfill_plan_hash.as_str().as_bytes(),
    );
    hasher.update(predecessor.applied_stamp_total.to_be_bytes());
    Sha256ValueV1::parse(hex::encode(hasher.finalize())).expect("code-owned digest is valid")
}

/// Canonical hash over the EXECUTABLE plan: the exact source binding the
/// rebuild will consume, in the scan's deterministic namespace order.
///
/// The proof mode and BOTH fingerprints are folded, not just the mode. Two
/// plans that agree on the mode but disagree on which index they proved it
/// against are different plans, and an identity check that could not tell
/// them apart would let artifacts captured against one index authorize an
/// apply against another.
pub fn rebuild_plan_hash(
    inventory_hash: &Sha256ValueV1,
    binding: &RebuildSourceBindingV1,
) -> Sha256ValueV1 {
    let mut hasher = Sha256::new();
    field(&mut hasher, b"blackbox.project-catalog.rebuild-plan.v1");
    field(&mut hasher, inventory_hash.as_str().as_bytes());
    field(
        &mut hasher,
        match binding.proof_mode {
            HistoryProofModeV1::Equality => b"equality".as_slice(),
            HistoryProofModeV1::Drift => b"drift".as_slice(),
        },
    );
    optional_hash_field(
        &mut hasher,
        binding.recorded_source_index_fingerprint.as_ref(),
    );
    optional_hash_field(
        &mut hasher,
        binding.observed_source_index_fingerprint.as_ref(),
    );
    match &binding.source_schema_version {
        Some(schema) => {
            field(&mut hasher, b"schema");
            field(&mut hasher, schema.as_bytes());
        }
        None => field(&mut hasher, b"no-schema"),
    }
    hasher.update((binding.namespace_dispositions.len() as u64).to_be_bytes());
    for row in &binding.namespace_dispositions {
        field(&mut hasher, row.namespace.as_bytes());
        hasher.update(row.commit_document_count.to_be_bytes());
        field(
            &mut hasher,
            row.commit_document_commitment_sha256.as_bytes(),
        );
        hasher.update(row.truncated_message_count.to_be_bytes());
    }
    Sha256ValueV1::parse(hex::encode(hasher.finalize())).expect("code-owned digest is valid")
}

/// The complete read-only rebuild plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathFreeRebuildPlanV1 {
    pub predecessor: RebuildPredecessorBindingV1,
    pub source_binding: RebuildSourceBindingV1,
    pub catalog_transaction_id: Option<ProjectCatalogTransactionId>,
    pub inventory_hash: Sha256ValueV1,
    pub plan_hash: Sha256ValueV1,
}

impl PathFreeRebuildPlanV1 {
    /// Render the reviewable report for this plan.
    ///
    /// `resolution_bytes` are the EXACT bytes of the resolution this report
    /// is bound to, hashed here rather than taken as a hash, so a caller
    /// cannot bind a report to a resolution it never read.
    pub fn report(&self, generated_at: String, resolution_bytes: &[u8]) -> PathFreeRebuildReportV1 {
        PathFreeRebuildReportV1 {
            version: PATH_FREE_REBUILD_REPORT_VERSION_V1,
            generated_at,
            // Reaching a report at all means the D-036 gate passed; a refused
            // plan returns its refusal instead of rendering.
            status: PathFreeRebuildStatusV1::Clean,
            inventory_hash: self.inventory_hash.clone(),
            plan_hash: self.plan_hash.clone(),
            resolution_artifact_hash: Sha256ValueV1::digest(resolution_bytes),
            predecessor: self.predecessor.clone(),
            source_binding: self.source_binding.clone(),
            planned_commit_document_total: self.source_binding.commit_document_total(),
        }
    }
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
/// The proof mode is NOT recomputed here. It comes from
/// `history_materializer::prove_source_index`, the same function apply's
/// materializer uses, called with the same request shape. This is the whole
/// point: preflight exists to predict what apply will decide, so a second
/// implementation of the derivation could report Equality and authorize a cut
/// whose apply then lands in Drift. One function makes that disagreement
/// impossible rather than merely unlikely, and it carries the cursor-root and
/// RESOLVED-vector-root reasoning (R33F1) with it instead of leaving a copy
/// here to drift.
pub fn plan_source_binding(
    layout: &ProjectCatalogMigrationResolvedLayoutV1,
    origin: &CatalogOriginV2,
    scan_limits: HistoryScanLimitsV1,
) -> PlanningResult<RebuildSourceBindingV1> {
    let index_path = layout.index_root_for_rebuild();
    let asset = match origin {
        CatalogOriginV2::FreshV2 {} => None,
        CatalogOriginV2::MigratedV1 { transaction_id } => Some(
            load_legacy_commit_namespace_inventory_asset(layout.projects_path(), transaction_id)?
                .ok_or_else(|| {
                refuse(
                    ERROR_REBUILD_PROOF_MODE,
                    format!(
                        "migrated catalog {transaction_id} has no persisted legacy \
                             commit-namespace inventory asset, so Equality cannot be proved \
                             and legacy history cannot be rematerialized unproved"
                    ),
                )
            })?,
        ),
    };
    // THE shared derivation. The request is built from the layout's own
    // resolved roots so the preflight proves against exactly the index,
    // cursor root, and vector store apply will consume.
    let proof = prove_source_index(
        asset.as_ref(),
        &HistoryMaterializerRequestV1 {
            index_path: index_path.to_path_buf(),
            projects_path: layout.projects_path().to_path_buf(),
            vector_root: layout.vector_root_for_rebuild().to_path_buf(),
            scan_limits,
        },
    );
    let parse_fingerprint = |value: Option<String>| -> PlanningResult<Option<Sha256ValueV1>> {
        value
            .map(|value| {
                Sha256ValueV1::parse(value).map_err(|error| {
                    refuse(
                        ERROR_REBUILD_PROOF_MODE,
                        format!("source index fingerprint is not a sha256 value: {error}"),
                    )
                })
            })
            .transpose()
    };
    let recorded = parse_fingerprint(proof.recorded_source_index_fingerprint)?;
    let observed = parse_fingerprint(proof.observed_source_index_fingerprint)?;
    let proof_mode = proof.proof_mode;

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

    let inventory_hash = rebuild_inventory_hash(&predecessor);
    let plan_hash = rebuild_plan_hash(&inventory_hash, &source_binding);
    Ok(PathFreeRebuildPlanV1 {
        predecessor,
        source_binding,
        catalog_transaction_id: match origin {
            CatalogOriginV2::MigratedV1 { transaction_id } => Some(transaction_id),
            CatalogOriginV2::FreshV2 {} => None,
        },
        inventory_hash,
        plan_hash,
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

    fn namespace(name: &str, count: u64, commitment: &str) -> RebuildNamespaceDispositionV1 {
        RebuildNamespaceDispositionV1 {
            namespace: name.to_string(),
            commit_document_count: count,
            commit_document_commitment_sha256: commitment.to_string(),
            truncated_message_count: 0,
        }
    }

    fn predecessor(epoch: u64) -> RebuildPredecessorBindingV1 {
        RebuildPredecessorBindingV1 {
            backfill_post_image_catalog_epoch: epoch,
            backfill_inventory_hash: hash(0x33),
            backfill_plan_hash: hash(0x44),
            applied_stamp_total: 12,
            observed_catalog_epoch: epoch,
        }
    }

    fn equality_binding() -> RebuildSourceBindingV1 {
        RebuildSourceBindingV1 {
            recorded_source_index_fingerprint: Some(hash(0xaa)),
            observed_source_index_fingerprint: Some(hash(0xaa)),
            proof_mode: HistoryProofModeV1::Equality,
            namespace_dispositions: vec![namespace("alpha", 3, "c1"), namespace("beta", 5, "c2")],
            source_schema_version: Some("schema-1".to_string()),
        }
    }

    /// The plan hash must move when ANY component of the executable plan
    /// moves. A hash that ignored a component would let artifacts captured
    /// against one plan authorize an apply of a different one.
    #[test]
    fn the_plan_hash_is_sensitive_to_every_plan_component() {
        let inventory = rebuild_inventory_hash(&predecessor(7));
        let baseline = rebuild_plan_hash(&inventory, &equality_binding());

        let mut drift = equality_binding();
        drift.proof_mode = HistoryProofModeV1::Drift;

        let mut other_index = equality_binding();
        other_index.observed_source_index_fingerprint = Some(hash(0xbb));

        let mut fewer_documents = equality_binding();
        fewer_documents.namespace_dispositions[0].commit_document_count = 2;

        let mut other_documents = equality_binding();
        other_documents.namespace_dispositions[0].commit_document_commitment_sha256 =
            "c9".to_string();

        let mut extra_namespace = equality_binding();
        extra_namespace
            .namespace_dispositions
            .push(namespace("gamma", 1, "c3"));

        let mut other_schema = equality_binding();
        other_schema.source_schema_version = Some("schema-2".to_string());

        for (label, mutated) in [
            ("proof mode", drift),
            ("observed fingerprint", other_index),
            ("document count", fewer_documents),
            ("document commitment", other_documents),
            ("namespace set", extra_namespace),
            ("source schema", other_schema),
        ] {
            assert_ne!(
                baseline,
                rebuild_plan_hash(&inventory, &mutated),
                "the plan hash must move when the {label} moves"
            );
        }

        // And a different predecessor is a different plan even when the
        // source binding is byte-identical.
        assert_ne!(
            baseline,
            rebuild_plan_hash(
                &rebuild_inventory_hash(&predecessor(8)),
                &equality_binding()
            )
        );
    }

    /// A present fingerprint must never collide with an absent one. Hashing
    /// the value alone would make `None` and `Some("")` indistinguishable.
    #[test]
    fn absent_and_present_fingerprints_do_not_collide() {
        let inventory = rebuild_inventory_hash(&predecessor(7));
        let mut absent = equality_binding();
        absent.recorded_source_index_fingerprint = None;
        absent.proof_mode = HistoryProofModeV1::Drift;
        let mut present = absent.clone();
        present.recorded_source_index_fingerprint = Some(hash(0x00));
        assert_ne!(
            rebuild_plan_hash(&inventory, &absent),
            rebuild_plan_hash(&inventory, &present)
        );
    }

    /// FD-4: the artifact hash graph is ACYCLIC. The report carries the
    /// resolution's hash; neither carries its own, and the resolution does
    /// not carry the report's.
    #[test]
    fn the_artifact_hash_graph_stays_acyclic() {
        let inventory = rebuild_inventory_hash(&predecessor(7));
        let plan = PathFreeRebuildPlanV1 {
            predecessor: predecessor(7),
            source_binding: equality_binding(),
            catalog_transaction_id: None,
            inventory_hash: inventory.clone(),
            plan_hash: rebuild_plan_hash(&inventory, &equality_binding()),
        };
        let resolution = PathFreeRebuildResolutionV1::empty(inventory.clone());
        let resolution_bytes = serde_json::to_vec(&resolution).unwrap();
        let report = plan.report("2026-08-04T00:00:00Z".to_string(), &resolution_bytes);

        assert_eq!(
            report.resolution_artifact_hash,
            Sha256ValueV1::digest(&resolution_bytes),
            "the report binds the EXACT resolution bytes it was given"
        );
        assert_eq!(report.planned_commit_document_total, 8);

        let report_bytes = serde_json::to_vec(&report).unwrap();
        let report_hash = Sha256ValueV1::digest(&report_bytes);
        let rendered = String::from_utf8(report_bytes).unwrap();
        assert!(
            !rendered.contains(report_hash.as_str()),
            "no artifact may contain its own byte hash"
        );
        let rendered_resolution = String::from_utf8(resolution_bytes).unwrap();
        assert!(
            !rendered_resolution.contains(report_hash.as_str()),
            "the resolution must not carry the report's hash"
        );

        // Both artifacts round-trip under deny_unknown_fields.
        let decoded: PathFreeRebuildReportV1 = serde_json::from_str(&rendered).unwrap();
        assert_eq!(decoded, report);
        let decoded: PathFreeRebuildResolutionV1 =
            serde_json::from_str(&rendered_resolution).unwrap();
        assert_eq!(decoded, resolution);
    }

    /// The proof-mode derivation must stay a CALL, never a copy.
    ///
    /// This module's whole reason for sharing `prove_source_index` is that a
    /// second derivation could report Equality at preflight and land in Drift
    /// at apply. That risk returns the moment someone reintroduces a local
    /// fingerprint comparison here, and it would return silently: the
    /// existing tests would all still pass, because a mirrored derivation
    /// agrees with the shared one right up until one of them changes.
    #[test]
    fn the_proof_mode_derivation_is_shared_not_mirrored() {
        let source = include_str!("project_catalog_rebuild_planning.rs");
        assert!(
            source.contains("prove_source_index("),
            "planning must call the shared proof-mode export"
        );
        // The body must not recompute a fingerprint or re-decide the mode.
        let body = source
            .split("#[cfg(test)]")
            .next()
            .expect("the module body precedes its tests");
        for copied in [
            "recompute_legacy_commit_namespace_source_fingerprint",
            "HistoryProofModeV1::Equality,",
        ] {
            assert!(
                !body.contains(copied),
                "planning must not restate `{copied}`: the proof-mode derivation \
                 belongs to history_materializer, never to a second copy"
            );
        }
    }
}

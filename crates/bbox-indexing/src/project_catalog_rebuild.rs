//! Closed public authority boundary for the Phase 6 path-free rebuild.
//!
//! This module holds the rebuild's VERIFICATION half (Phase 6 plan section 3.4
//! and milestone P6-B task 6). It is deliberately a reader: the rebuild's
//! creation path is P3-D's (`materialize_history_generations`,
//! `prepare_rebuild_manifest`, `write_prepared_rebuild_manifest`) and its
//! replacement pass is P3-E's, and the plan forbids a parallel manifest writer
//! because a second implementation would fork the crash and recovery semantics
//! P3-D owns.
//!
//! The same rule governs REPAIR. When a committed manifest's recorded proof is
//! attributable to a schema-version transition, the manifest is re-founded by
//! P3-D's `refound_committed_rebuild_manifest`, which owns that write and its
//! crash window; this module keeps refusing until that has happened, and never
//! rewrites a manifest itself.
//!
//! Two contracts are enforced here and nowhere else in this crate:
//!
//! - **Equality proof mode is mandatory (D-036).** A `Drift`-mode outcome
//!   proves only that no recorded history was LOST, not that the asset
//!   enumerates everything present. That is too weak to authorize a cut, so a
//!   committed manifest recording anything but `Equality` refuses with
//!   `error.project_catalog_rebuild_proof_mode`.
//! - **Every manifest bucket is verified (D-037).** Compatibility-namespace
//!   generations have no catalog `Ready` owner and are reachable ONLY through
//!   the manifest's `compatibility_generation_ids` bucket. A verifier that
//!   walked `Ready` requirements would silently skip them, so this walks the
//!   manifest's own buckets: primary owned, compatibility, ambiguous, and
//!   unclaimed.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use bbox_corpus_index::index::history_generations::{
    HistoryGenerationIdV1, HistoryGenerationStore, HistoryProofModeV1,
    RepoHistoryRebuildDispositionV1, RepoHistoryRebuildManifestV1, RepoHistoryRebuildStateV1,
};
use serde::Serialize;

use crate::project_catalog_migration::{
    ProjectCatalogMigrationError, ProjectCatalogMigrationResolvedLayoutV1,
    ProjectCatalogTargetSelectionV1, validate_target_selection,
};

/// The plan's NEW proof-mode refusal (section 7.3). Raised by rebuild
/// apply/verify and by the P6-C startup gate on cut-time manifests.
pub const ERROR_REBUILD_PROOF_MODE: &str = "error.project_catalog_rebuild_proof_mode";
/// The plan's NEW missing-manifest refusal (section 7.3).
pub const ERROR_REBUILD_MANIFEST_MISSING: &str = "error.project_catalog_rebuild_manifest_missing";
/// The plan's NEW unverified-generation refusal (section 7.3).
pub const ERROR_REBUILD_GENERATION_UNVERIFIED: &str =
    "error.project_catalog_rebuild_generation_unverified";

type RebuildResult<T> = Result<T, ProjectCatalogMigrationError>;

fn refuse(code: &'static str, message: impl Into<String>) -> ProjectCatalogMigrationError {
    ProjectCatalogMigrationError::no_mutation(code, message)
}

/// Which manifest bucket a generation was verified through.
///
/// Recorded per generation rather than aggregated, because "how many
/// generations verified" cannot distinguish a complete walk from one that
/// skipped a whole bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildManifestBucketV1 {
    /// The primary owned set: generations a repo-history record's
    /// `materialization` names.
    Owned,
    /// Catalog-ATTRIBUTED but manifest-OWNED: no catalog record names these,
    /// so the manifest is their only durable identity and GC root (D-037).
    Compatibility,
    /// Ambiguous namespaces, tracked by `RepoHistoryQuarantineGenerationId`.
    Ambiguous,
    /// Unclaimed namespaces. The manifest is likewise their only owner.
    Unclaimed,
}

impl RebuildManifestBucketV1 {
    fn of(disposition: RepoHistoryRebuildDispositionV1) -> Self {
        match disposition {
            RepoHistoryRebuildDispositionV1::Owned => Self::Owned,
            RepoHistoryRebuildDispositionV1::OwnedCompatibility => Self::Compatibility,
            RepoHistoryRebuildDispositionV1::Ambiguous => Self::Ambiguous,
            RepoHistoryRebuildDispositionV1::Unclaimed => Self::Unclaimed,
        }
    }

    /// Whether generations in this bucket carry quarantine (`rhq_`) ids.
    ///
    /// Prefix discipline (plan P6-E item 5): owned and compatibility
    /// generations carry `rhg_` ids; quarantine generations carry `rhq_`. A
    /// verifier expecting `rhg_` on a quarantine generation would reject valid
    /// state, so the expectation is derived from the bucket rather than
    /// assumed.
    fn expects_quarantine_id(self) -> bool {
        matches!(self, Self::Ambiguous | Self::Unclaimed)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VerifiedGenerationV1 {
    pub generation_id: String,
    pub bucket: RebuildManifestBucketV1,
    pub commit_document_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PathFreeRebuildVerifyReceiptV1 {
    pub rebuild_id: String,
    pub proof_mode: HistoryProofModeV1,
    pub recorded_source_index_fingerprint: Option<String>,
    pub observed_source_index_fingerprint: Option<String>,
    pub resulting_catalog_epoch: u64,
    pub verified_generations: Vec<VerifiedGenerationV1>,
    pub bucket_counts: BTreeMap<RebuildManifestBucketV1, u64>,
}

pub struct PathFreeRebuildVerifyRequestV1 {
    /// ONE explicit target layout. Isolation is the caller's responsibility,
    /// expressed by which layout it hands in; the facade runs no
    /// rehearsal-versus-protected separation check.
    pub layout: ProjectCatalogMigrationResolvedLayoutV1,
    /// Which target the CLI resolved. Verify is target-explicit under the same
    /// selection rules as apply (section 3.1).
    pub target_selection: ProjectCatalogTargetSelectionV1,
}

/// Read the committed rebuild manifest for an index root.
pub fn read_committed_rebuild_manifest(
    index_path: &Path,
) -> RebuildResult<RepoHistoryRebuildManifestV1> {
    let store = HistoryGenerationStore::open_for_index(index_path)
        .map_err(|error| refuse(ERROR_REBUILD_MANIFEST_MISSING, error.to_string()))?;
    let Some(manifest) = store
        .read_rebuild_manifest()
        .map_err(|error| refuse(ERROR_REBUILD_MANIFEST_MISSING, error.to_string()))?
    else {
        return Err(refuse(
            ERROR_REBUILD_MANIFEST_MISSING,
            "no rebuild manifest is present for this index root",
        ));
    };
    if manifest.state != RepoHistoryRebuildStateV1::Committed {
        return Err(refuse(
            ERROR_REBUILD_MANIFEST_MISSING,
            "rebuild manifest is prepared but not committed",
        ));
    }
    Ok(manifest)
}

/// Enforce D-036 against a manifest's recorded proof mode.
///
/// Both fingerprints are recorded even in `Equality` mode so a later audit can
/// re-derive the mode decision instead of trusting it; this re-derives it. A
/// manifest claiming `Equality` whose two fingerprints disagree is refused with
/// the same code, because the claim and its own evidence contradict.
///
/// The check is deliberately basis-agnostic. `HistoryProofBasisV1` records
/// WHICH predecessor decided the mode, and both bases answer the same question
/// - is this inventory an exact enumeration of its basis - from a pair of
/// fingerprints that is re-derivable from durable state. D-036 is a statement
/// about the strength of the answer, not about which predecessor supplied it,
/// so this enforces the mode and leaves the basis to the writer that could
/// actually prove it.
pub fn require_equality_proof_mode(manifest: &RepoHistoryRebuildManifestV1) -> RebuildResult<()> {
    if manifest.prepared.proof_mode != HistoryProofModeV1::Equality {
        return Err(refuse(
            ERROR_REBUILD_PROOF_MODE,
            "committed rebuild manifest does not record the mandatory Equality proof mode",
        ));
    }
    match (
        manifest.prepared.recorded_source_index_fingerprint.as_ref(),
        manifest.prepared.observed_source_index_fingerprint.as_ref(),
    ) {
        (Some(recorded), Some(observed)) if recorded == observed => Ok(()),
        _ => Err(refuse(
            ERROR_REBUILD_PROOF_MODE,
            "committed rebuild manifest claims Equality without two matching source fingerprints",
        )),
    }
}

/// Verify every generation the manifest names, in EVERY bucket.
///
/// `HistoryGenerationStore::load` re-derives the record's own commitments and
/// refuses a mismatch, so loading is the hash verification; this additionally
/// proves the loaded generation agrees with the manifest ROW's recorded count
/// and commitment, which is what makes the manifest, not the on-disk bytes, the
/// authority for what was supposed to be there.
pub fn verify_manifest_generations(
    index_path: &Path,
    manifest: &RepoHistoryRebuildManifestV1,
) -> RebuildResult<Vec<VerifiedGenerationV1>> {
    let store = HistoryGenerationStore::open_for_index(index_path)
        .map_err(|error| refuse(ERROR_REBUILD_GENERATION_UNVERIFIED, error.to_string()))?;

    // The namespace inventory is the manifest's COMPLETE claim; the four id
    // buckets are its disposition index. Verifying the inventory and then
    // proving the buckets are exactly covered by it catches a manifest whose
    // bucket names a generation the inventory never described, which a walk of
    // either side alone would miss.
    let mut verified = Vec::new();
    let mut inventory_ids = BTreeSet::new();
    for row in &manifest.prepared.namespace_inventory {
        let bucket = RebuildManifestBucketV1::of(row.disposition);
        let id = HistoryGenerationIdV1::parse(&row.generation_id)
            .map_err(|error| refuse(ERROR_REBUILD_GENERATION_UNVERIFIED, error.to_string()))?;
        if id.quarantine().is_some() != bucket.expects_quarantine_id() {
            return Err(refuse(
                ERROR_REBUILD_GENERATION_UNVERIFIED,
                format!(
                    "generation {} carries an id whose prefix disagrees with its manifest bucket",
                    row.generation_id
                ),
            ));
        }
        let record = store.load(&id).map_err(|error| {
            refuse(
                ERROR_REBUILD_GENERATION_UNVERIFIED,
                format!(
                    "rebuild manifest names generation {} which cannot be verified: {error}",
                    row.generation_id
                ),
            )
        })?;
        if record.manifest.body.commit_document_count != row.commit_document_count
            || record.manifest.body.commit_document_commitment_sha256
                != row.commit_document_commitment_sha256
        {
            return Err(refuse(
                ERROR_REBUILD_GENERATION_UNVERIFIED,
                format!(
                    "generation {} disagrees with its rebuild manifest row",
                    row.generation_id
                ),
            ));
        }
        inventory_ids.insert(row.generation_id.clone());
        verified.push(VerifiedGenerationV1 {
            generation_id: row.generation_id.clone(),
            bucket,
            commit_document_count: row.commit_document_count,
        });
    }

    // Every bucket must be covered by the inventory just verified. The
    // compatibility bucket is called out because it is the one with no catalog
    // record naming it: dropping it here would leave it unverified AND
    // sweepable, since the manifest is its only GC root.
    for (bucket_name, ids) in [
        ("owned", &manifest.prepared.owned_generation_ids),
        (
            "compatibility",
            &manifest.prepared.compatibility_generation_ids,
        ),
        ("ambiguous", &manifest.prepared.ambiguous_generation_ids),
        ("unclaimed", &manifest.prepared.unclaimed_generation_ids),
    ] {
        for id in ids {
            if !inventory_ids.contains(id) {
                return Err(refuse(
                    ERROR_REBUILD_GENERATION_UNVERIFIED,
                    format!(
                        "the {bucket_name} bucket names generation {id}, which the manifest's \
                         namespace inventory does not describe"
                    ),
                ));
            }
        }
    }
    verified.sort_by(|left, right| left.generation_id.cmp(&right.generation_id));
    Ok(verified)
}

/// The only public executable path-free-rebuild verification authority.
pub struct ProjectCatalogPathFreeRebuildFacadeV1;

impl ProjectCatalogPathFreeRebuildFacadeV1 {
    /// Fresh verification against durable state (task 6).
    ///
    /// Checks that the manifest is `Committed`, that it records
    /// `HistoryProofModeV1::Equality` with matching source fingerprints
    /// (D-036), and that every named generation is present and hash-verified in
    /// EVERY manifest bucket (D-037). It reads durable records, never operator
    /// artifacts: the committed manifest already carries the evidence it was
    /// applied from.
    pub fn verify(
        request: PathFreeRebuildVerifyRequestV1,
    ) -> RebuildResult<PathFreeRebuildVerifyReceiptV1> {
        // Q-C binding condition. Verify takes no artifacts, so only the
        // selection condition applies: the selected target must match the
        // layout's real shape before any durable record is read.
        validate_target_selection(&request.layout, request.target_selection)?;
        let index_path = request.layout.index_root_for_rebuild().to_path_buf();
        let manifest = read_committed_rebuild_manifest(&index_path)?;
        require_equality_proof_mode(&manifest)?;
        let verified = verify_manifest_generations(&index_path, &manifest)?;
        let mut bucket_counts: BTreeMap<RebuildManifestBucketV1, u64> = BTreeMap::new();
        for row in &verified {
            *bucket_counts.entry(row.bucket).or_default() += 1;
        }
        let resulting_catalog_epoch = manifest
            .committed
            .as_ref()
            .map(|committed| committed.resulting_catalog_epoch)
            .ok_or_else(|| {
                refuse(
                    ERROR_REBUILD_MANIFEST_MISSING,
                    "a committed rebuild manifest carries no committed block",
                )
            })?;
        Ok(PathFreeRebuildVerifyReceiptV1 {
            rebuild_id: manifest.rebuild_id.clone(),
            proof_mode: manifest.prepared.proof_mode,
            recorded_source_index_fingerprint: manifest
                .prepared
                .recorded_source_index_fingerprint
                .clone(),
            observed_source_index_fingerprint: manifest
                .prepared
                .observed_source_index_fingerprint
                .clone(),
            resulting_catalog_epoch,
            verified_generations: verified,
            bucket_counts,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bbox_corpus_core::project_catalog::CommitNamespace;
    use bbox_corpus_index::index::history_generations::{
        HistoryProofBasisV1, RepoHistoryRebuildNamespaceV1, RepoHistoryRebuildPreparedV1,
    };

    fn prepared(proof_mode: HistoryProofModeV1) -> RepoHistoryRebuildPreparedV1 {
        RepoHistoryRebuildPreparedV1 {
            source_index_fingerprint_sha256: "a".repeat(64),
            source_schema_version: "schema-v1".to_string(),
            proof_mode,
            proof_basis: HistoryProofBasisV1::MigrationAsset,
            recorded_source_index_fingerprint: Some("f".repeat(64)),
            observed_source_index_fingerprint: Some("f".repeat(64)),
            namespace_inventory: Vec::new(),
            catalog_epoch: 3,
            owned_generation_ids: BTreeSet::new(),
            compatibility_generation_ids: BTreeSet::new(),
            ambiguous_generation_ids: BTreeSet::new(),
            unclaimed_generation_ids: BTreeSet::new(),
            planned_lexical_generation_label: "lexical:next".to_string(),
            planned_vector_generation_label: "vector:next".to_string(),
        }
    }

    fn manifest_with(prepared: RepoHistoryRebuildPreparedV1) -> RepoHistoryRebuildManifestV1 {
        RepoHistoryRebuildManifestV1 {
            version: 1,
            rebuild_id: "rhb_test".to_string(),
            state: RepoHistoryRebuildStateV1::Committed,
            prepared,
            committed: None,
        }
    }

    fn namespace_row(
        generation_id: &str,
        disposition: RepoHistoryRebuildDispositionV1,
    ) -> RepoHistoryRebuildNamespaceV1 {
        RepoHistoryRebuildNamespaceV1 {
            namespace: CommitNamespace::parse("legacy-repo").unwrap(),
            generation_id: generation_id.to_string(),
            commit_document_count: 1,
            commit_document_commitment_sha256: "c".repeat(64),
            disposition,
        }
    }

    /// D-036: Drift proves only non-loss, which is insufficient for a
    /// cut-authorizing rebuild.
    #[test]
    fn a_drift_mode_manifest_refuses() {
        let error =
            require_equality_proof_mode(&manifest_with(prepared(HistoryProofModeV1::Drift)))
                .unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_PROOF_MODE);
    }

    #[test]
    fn an_equality_mode_manifest_with_matching_fingerprints_passes() {
        require_equality_proof_mode(&manifest_with(prepared(HistoryProofModeV1::Equality)))
            .unwrap();
    }

    /// A manifest may not merely CLAIM Equality: the mode decision is
    /// re-derived from the two fingerprints it records.
    #[test]
    fn an_equality_claim_with_disagreeing_fingerprints_refuses() {
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.observed_source_index_fingerprint = Some("0".repeat(64));
        let error = require_equality_proof_mode(&manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_PROOF_MODE);
    }

    /// An Equality claim with no fingerprints at all is unfalsifiable and is
    /// refused rather than trusted.
    #[test]
    fn an_equality_claim_without_fingerprints_refuses() {
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.recorded_source_index_fingerprint = None;
        body.observed_source_index_fingerprint = None;
        let error = require_equality_proof_mode(&manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_PROOF_MODE);
    }

    /// Prefix discipline (P6-E item 5): quarantine buckets carry `rhq_` ids,
    /// owned and compatibility carry `rhg_`. A quarantine id in the owned
    /// bucket is rejected before any load is attempted.
    #[test]
    fn a_bucket_prefix_disagreement_refuses() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.namespace_inventory = vec![namespace_row(
            &format!("rhq_{}", "a".repeat(64)),
            RepoHistoryRebuildDispositionV1::Owned,
        )];
        let error = verify_manifest_generations(&index_path, &manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_GENERATION_UNVERIFIED);
        assert!(error.message.contains("prefix"));
    }

    /// The reverse direction: an owned-shaped id in the ambiguous bucket is
    /// equally wrong. Asserting only one direction would let a verifier that
    /// hard-coded `rhg_` pass.
    #[test]
    fn an_owned_id_in_a_quarantine_bucket_refuses() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.namespace_inventory = vec![namespace_row(
            &format!("rhg_{}", "a".repeat(64)),
            RepoHistoryRebuildDispositionV1::Ambiguous,
        )];
        let error = verify_manifest_generations(&index_path, &manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_GENERATION_UNVERIFIED);
    }

    /// D-037: a compatibility generation has no catalog `Ready` owner, so a
    /// bucket entry the namespace inventory never described would otherwise go
    /// unverified AND stay sweepable.
    #[test]
    fn a_compatibility_bucket_entry_outside_the_inventory_refuses() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.compatibility_generation_ids = BTreeSet::from([format!("rhg_{}", "b".repeat(64))]);
        let error = verify_manifest_generations(&index_path, &manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_GENERATION_UNVERIFIED);
        assert!(error.message.contains("compatibility"));
    }

    /// A generation named by the manifest but absent from disk refuses; the
    /// verifier never treats an unreadable generation as vacuously fine.
    #[test]
    fn an_absent_generation_refuses() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let mut body = prepared(HistoryProofModeV1::Equality);
        body.namespace_inventory = vec![namespace_row(
            &format!("rhg_{}", "a".repeat(64)),
            RepoHistoryRebuildDispositionV1::Owned,
        )];
        let error = verify_manifest_generations(&index_path, &manifest_with(body)).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_GENERATION_UNVERIFIED);
    }

    /// A prepared-but-not-committed manifest is not a verifiable rebuild.
    #[test]
    fn a_prepared_manifest_is_not_a_committed_one() {
        let root = tempfile::tempdir().unwrap();
        let index_path = root.path().join("index");
        std::fs::create_dir_all(&index_path).unwrap();
        let error = read_committed_rebuild_manifest(&index_path).unwrap_err();
        assert_eq!(error.code, ERROR_REBUILD_MANIFEST_MISSING);
    }

    /// Every disposition maps to its own bucket, and only the quarantine
    /// dispositions expect `rhq_` ids.
    #[test]
    fn buckets_map_dispositions_and_prefix_expectations() {
        for (disposition, bucket, quarantine) in [
            (
                RepoHistoryRebuildDispositionV1::Owned,
                RebuildManifestBucketV1::Owned,
                false,
            ),
            (
                RepoHistoryRebuildDispositionV1::OwnedCompatibility,
                RebuildManifestBucketV1::Compatibility,
                false,
            ),
            (
                RepoHistoryRebuildDispositionV1::Ambiguous,
                RebuildManifestBucketV1::Ambiguous,
                true,
            ),
            (
                RepoHistoryRebuildDispositionV1::Unclaimed,
                RebuildManifestBucketV1::Unclaimed,
                true,
            ),
        ] {
            assert_eq!(RebuildManifestBucketV1::of(disposition), bucket);
            assert_eq!(bucket.expects_quarantine_id(), quarantine);
        }
    }
}

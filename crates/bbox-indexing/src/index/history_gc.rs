//! History generation GC: the derived reference manifest and the
//! generation-driven vector tombstone path (Phase 3 plan section 10 item 4;
//! governing section 11 and section 16).
//!
//! THE MANIFEST IS AN ACCELERATION INDEX, NOT AUTHORITY (D-038). Its durable
//! inputs - the persisted catalog records and the active/retained Git overlay
//! selectors in the edge sidecar's manifest index - ARE the authority. It is
//! rebuilt from those inputs at startup and before EVERY GC pass, and the
//! rebuild always wins.
//!
//! That single sentence decides the divergence policy, and the first cut of
//! this module got it backwards. It treated a checksum mismatch as evidence
//! of corruption and disabled GC without persisting the rebuild. But NO
//! sanctioned mutation path writes this file: an overlay swap and a
//! `Ready` materialization advance both change durable inputs and neither
//! refreshes the acceleration index. So the very first legitimate history
//! operation after baselining produced a mismatch, and because the mismatch
//! arm did not persist, every later evaluation re-derived the same mismatch
//! against the same stale bytes. GC latched off permanently behind a doctor
//! finding that described normal operation as corruption, recoverable only by
//! deleting the file by hand.
//!
//! The policy is therefore accept-and-persist: a persisted manifest that
//! decodes but disagrees is STALE, not suspect. The rebuild replaces it, the
//! divergence is logged with both checksums and carried as an informational
//! note, and GC stays enabled. `Disabled` survives for exactly two arms, both
//! of which mean the evaluation could not be performed at all rather than
//! that its answer was surprising:
//!
//!   1. the rebuild's own inputs are unreadable (catalog, generation store,
//!      or an I/O error reaching the persisted file);
//!   2. the persisted file exists but cannot be DECODED - genuine unexplained
//!      corruption of bytes this daemon wrote.
//!
//! A stale acceleration index may cost a sweep; it must never cost a
//! generation, and it must never cost the ability to sweep again.
//!
//! WHY A CRASH BETWEEN AN OVERLAY SWAP AND A MANIFEST REFRESH IS SAFE. The
//! overlay selector is written atomically into the workspace manifest entry
//! inside the manifest coordinator, and that entry is a DURABLE INPUT to this
//! rebuild rather than something the reference manifest caches independently.
//! So a process that dies after the swap and before any refresh still
//! recomputes a reference set containing the swapped-in generation the next
//! time it starts: the generation is never unreferenced in the window, and
//! the next evaluation converges by persisting the rebuild.
//!
//! IN-PROCESS ROOTS. Pinned read views and in-flight builds are added to the
//! rebuilt durable set while the process runs. They cannot be persisted (they
//! do not survive the process that holds them) and they do not need to be: a
//! restart cannot be holding them.
//!
//! NO PRODUCTION SWEEP THIS PHASE. `plan_history_gc` and
//! `tombstone_generation_vectors` deliberately have no production caller:
//! Phase 3 ships the enablement evaluation and the machinery only. The
//! destructive pass that consumes them is a later, separately authorized
//! wiring, and nothing may sweep this root except a caller holding a root set
//! from an `Enabled` evaluation.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use bbox_corpus_core::git_overlay::GitOverlaySelector;
use bbox_corpus_core::project_catalog::{
    CatalogSnapshotV2, RepoHistoryMaterialization, RepoHistoryQuarantineMaterialization,
};
use bbox_corpus_index::index::history_generations::{
    HistoryGenerationIdV1, HistoryGenerationRecordV1, HistoryGenerationStore,
    RepoHistoryRebuildManifestV1,
};

const REFERENCE_MANIFEST_FILE: &str = "reference-manifest.json";
const REFERENCE_MANIFEST_VERSION_V1: u32 = 1;
const REFERENCE_MANIFEST_DOMAIN: &[u8] = b"blackbox.repo-history-reference-manifest.v1";

/// Why a generation is referenced. Recorded per generation so a doctor
/// finding can say WHICH holder is keeping it alive instead of only that
/// something is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryReferenceKindV1 {
    /// A `RepoHistoryRecord.materialization` names it.
    CatalogRecord,
    /// An `AmbiguousNamespaceRecord.materialization` names it.
    QuarantineRecord,
    /// A project's selected Git current-file overlay names it.
    ActiveOverlay,
    /// A prepared or committed rebuild manifest pins it.
    RebuildManifest,
    /// A pinned read view in THIS process holds it. Never persisted.
    PinnedReadView,
    /// An in-flight history build in THIS process holds it. Never persisted.
    InFlightBuild,
}

impl HistoryReferenceKindV1 {
    fn as_str(self) -> &'static str {
        match self {
            Self::CatalogRecord => "catalog_record",
            Self::QuarantineRecord => "quarantine_record",
            Self::ActiveOverlay => "active_overlay",
            Self::RebuildManifest => "rebuild_manifest",
            Self::PinnedReadView => "pinned_read_view",
            Self::InFlightBuild => "in_flight_build",
        }
    }

    /// Whether this reference kind can be recomputed after a restart.
    ///
    /// The two process-local kinds are excluded from the persisted checksum
    /// because they legitimately differ between the writer and any later
    /// reader; including them would make every restart look like a mismatch
    /// and disable GC permanently.
    fn is_durable(self) -> bool {
        !matches!(self, Self::PinnedReadView | Self::InFlightBuild)
    }
}

/// The derived reference set. Rebuilt from durable inputs; never trusted as
/// the authority for whether a generation may be swept.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HistoryReferenceManifestV1 {
    pub version: u32,
    pub catalog_epoch: u64,
    /// generation id -> the reasons it is referenced.
    pub references: BTreeMap<String, BTreeSet<HistoryReferenceKindV1>>,
    /// Over the DURABLE references only; see `HistoryReferenceKindV1::is_durable`.
    pub checksum_sha256: String,
}

impl HistoryReferenceManifestV1 {
    /// Every referenced generation id: the GC root set.
    pub fn roots(&self) -> BTreeSet<String> {
        self.references.keys().cloned().collect()
    }

    fn recompute_checksum(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(REFERENCE_MANIFEST_DOMAIN);
        hasher.update(self.catalog_epoch.to_be_bytes());
        for (generation_id, kinds) in &self.references {
            let durable: Vec<&'static str> = kinds
                .iter()
                .copied()
                .filter(|kind| kind.is_durable())
                .map(HistoryReferenceKindV1::as_str)
                .collect();
            if durable.is_empty() {
                continue;
            }
            hasher.update((generation_id.len() as u64).to_be_bytes());
            hasher.update(generation_id.as_bytes());
            for kind in durable {
                hasher.update((kind.len() as u64).to_be_bytes());
                hasher.update(kind.as_bytes());
            }
        }
        hex::encode(hasher.finalize())
    }
}

/// Rebuild the reference manifest from durable inputs plus this process's
/// in-memory roots.
///
/// `pinned_read_view_generations` and `in_flight_build_generations` are the
/// process-local additions governing section 11 requires; passing empty sets
/// is correct for a startup rebuild, where no view is pinned yet.
pub fn build_reference_manifest(
    catalog: &CatalogSnapshotV2,
    overlays: &BTreeMap<String, GitOverlaySelector>,
    rebuild_manifests: &[RepoHistoryRebuildManifestV1],
    pinned_read_view_generations: &BTreeSet<String>,
    in_flight_build_generations: &BTreeSet<String>,
) -> HistoryReferenceManifestV1 {
    let mut references: BTreeMap<String, BTreeSet<HistoryReferenceKindV1>> = BTreeMap::new();
    let mut add = |id: String, kind: HistoryReferenceKindV1| {
        references.entry(id).or_default().insert(kind);
    };
    for record in catalog.repo_histories.values() {
        if let RepoHistoryMaterialization::Ready { generation_id } = &record.materialization {
            add(
                generation_id.as_str().to_string(),
                HistoryReferenceKindV1::CatalogRecord,
            );
        }
    }
    for record in catalog.ambiguous_namespaces.values() {
        if let RepoHistoryQuarantineMaterialization::Ready { generation_id } =
            &record.materialization
        {
            add(
                generation_id.as_str().to_string(),
                HistoryReferenceKindV1::QuarantineRecord,
            );
        }
    }
    // The overlay input is what makes "retiring one sibling never tombstones
    // shared history" true: a retired project's overlay disappears with its
    // manifest entry, but every other member's overlay still names the same
    // repo-history generation, so the generation keeps a reference.
    for overlay in overlays.values() {
        add(
            overlay.repo_history_generation.clone(),
            HistoryReferenceKindV1::ActiveOverlay,
        );
    }
    for manifest in rebuild_manifests {
        for id in manifest.pinned_generation_ids() {
            add(id, HistoryReferenceKindV1::RebuildManifest);
        }
    }
    for id in pinned_read_view_generations {
        add(id.clone(), HistoryReferenceKindV1::PinnedReadView);
    }
    for id in in_flight_build_generations {
        add(id.clone(), HistoryReferenceKindV1::InFlightBuild);
    }
    let mut manifest = HistoryReferenceManifestV1 {
        version: REFERENCE_MANIFEST_VERSION_V1,
        catalog_epoch: catalog.epoch,
        references,
        checksum_sha256: String::new(),
    };
    manifest.checksum_sha256 = manifest.recompute_checksum();
    manifest
}

fn reference_manifest_path(store: &HistoryGenerationStore) -> PathBuf {
    store.root().join(REFERENCE_MANIFEST_FILE)
}

pub fn write_reference_manifest(
    store: &HistoryGenerationStore,
    manifest: &HistoryReferenceManifestV1,
) -> anyhow::Result<()> {
    let path = reference_manifest_path(store);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(manifest)?)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

pub fn read_reference_manifest(
    store: &HistoryGenerationStore,
) -> anyhow::Result<Option<HistoryReferenceManifestV1>> {
    match fs::read(reference_manifest_path(store)) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// A persisted acceleration index that decoded but disagreed with the
/// rebuild, and was replaced by it.
///
/// Informational by construction: the rebuild is authoritative, so this
/// records WHAT was replaced rather than a condition anyone must act on. It
/// carries both checksums because the ordinary cause (a durable input
/// changed through a sanctioned path that does not write this file) and the
/// alarming one (something edited the generations root out of band) are
/// indistinguishable from the enablement decision alone, and an operator
/// chasing the second needs the values to compare against their own records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryReferenceDivergenceV1 {
    pub persisted_version: u32,
    pub rebuilt_version: u32,
    pub persisted_checksum_sha256: String,
    pub rebuilt_checksum_sha256: String,
    /// Doctor-facing prose. Rendered as info, never as a failure.
    pub note: String,
}

/// Whether history GC may run this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryGcEnablementV1 {
    Enabled {
        roots: BTreeSet<String>,
        /// `Some` when a stale persisted index was accepted and replaced.
        /// A caller that ignores it loses only an explanation, never
        /// correctness: `roots` is derived from the durable inputs either
        /// way.
        divergence: Option<HistoryReferenceDivergenceV1>,
    },
    /// GC could not be evaluated this pass. `diagnostic` is doctor-facing
    /// prose. NOT reachable merely because the persisted index was stale;
    /// see the module docs for the two surviving arms.
    Disabled { diagnostic: String },
}

/// Rebuild the acceleration index, reconcile it against the persisted one,
/// and decide whether history GC may run (D-038).
///
/// The rebuild ALWAYS wins. Three outcomes, and only the first two exist in
/// normal operation:
///
/// - no persisted manifest: baseline it and enable. This is the first pass on
///   a store that predates the field.
/// - persisted manifest decodes: enable, persisting the rebuild either way.
///   When the two disagree, the persisted bytes were STALE - the ordinary
///   cause is a durable input that changed through a sanctioned path (an
///   overlay swap, a `Ready` advancement), none of which write this file - so
///   the divergence is logged with both checksums and returned as a note
///   rather than treated as corruption.
/// - the evaluation could not be performed: the persisted file is unreachable
///   (I/O error) or undecodable. Only then is GC disabled.
///
/// A FAILED WRITE does not disable GC. `roots` is derived from the durable
/// inputs, not from the file, so failing to refresh the cache costs nothing
/// this pass and the next pass re-derives and retries. Disabling on it would
/// reintroduce a milder version of the latch this policy exists to remove.
pub fn evaluate_history_gc(
    store: &HistoryGenerationStore,
    rebuilt: &HistoryReferenceManifestV1,
) -> HistoryGcEnablementV1 {
    let persisted = match read_reference_manifest(store) {
        Ok(persisted) => persisted,
        Err(error) => {
            return HistoryGcEnablementV1::Disabled {
                diagnostic: format!(
                    "the repo-history reference manifest could not be read or decoded \
                     ({error}); history GC is disabled until the generations root is \
                     inspected"
                ),
            };
        }
    };

    // The epoch is deliberately NOT part of the comparison: a catalog
    // mutation that changes no history reference legitimately bumps it, and
    // treating that as drift would report divergence on every unrelated
    // write. Version is compared because a differing version means the two
    // checksums were computed by different rules and are not comparable at
    // all - which is itself a divergence, resolved the same way.
    let divergence = persisted.and_then(|persisted| {
        if persisted.version == rebuilt.version
            && persisted.checksum_sha256 == rebuilt.checksum_sha256
        {
            return None;
        }
        let short = |value: &str| value[..value.len().min(12)].to_string();
        let note = format!(
            "the persisted repo-history reference index was stale (version {} checksum {}) \
             and was replaced by the rebuild (version {} checksum {}); the rebuild is \
             derived from the catalog and overlay selectors, which are the authority",
            persisted.version,
            short(&persisted.checksum_sha256),
            rebuilt.version,
            short(&rebuilt.checksum_sha256),
        );
        tracing::warn!(
            persisted_version = persisted.version,
            rebuilt_version = rebuilt.version,
            persisted_checksum = %persisted.checksum_sha256,
            rebuilt_checksum = %rebuilt.checksum_sha256,
            "the persisted repo-history reference index diverged from the rebuild; \
             accepting the rebuild"
        );
        Some(HistoryReferenceDivergenceV1 {
            persisted_version: persisted.version,
            rebuilt_version: rebuilt.version,
            persisted_checksum_sha256: persisted.checksum_sha256,
            rebuilt_checksum_sha256: rebuilt.checksum_sha256.clone(),
            note,
        })
    });

    if let Err(error) = write_reference_manifest(store, rebuilt) {
        // Non-fatal by policy; see the doc comment. Logged so a persistently
        // unwritable root is still visible rather than silently re-derived
        // forever.
        tracing::warn!(
            %error,
            "the repo-history reference index could not be refreshed; history GC stays \
             enabled because its roots come from the durable inputs, not this file"
        );
    }
    HistoryGcEnablementV1::Enabled {
        roots: rebuilt.roots(),
        divergence,
    }
}

/// A sink for vector tombstones, so the sweep is testable without a live
/// vector store and so the single-writer discipline stays at the call site.
pub trait HistoryVectorTombstoneSink {
    fn delete_entities_all_routes(&self, entity_ids: &[String]) -> anyhow::Result<()>;
}

/// Tombstone the vectors of ONE retired history generation.
///
/// Batches the generation's OWN deduplicated vector-input inventory into one
/// sink call. It never derives the entity set
/// from a project code selector: commit vectors are repo-scoped and enqueued
/// once per repository, so a project-selector-driven delete would either miss
/// them entirely (they carry no project) or, for a monorepo, delete a
/// sibling's live commit vectors on the first project's retirement.
pub fn tombstone_generation_vectors(
    generation: &HistoryGenerationRecordV1,
    sink: &dyn HistoryVectorTombstoneSink,
) -> anyhow::Result<u64> {
    let entity_ids = generation
        .vector_inputs
        .iter()
        .map(|input| input.entity_id.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    sink.delete_entities_all_routes(&entity_ids)?;
    Ok(entity_ids.len() as u64)
}

/// Generations on disk that the root set does not name.
///
/// `HistoryGenerationStore::remove_unreferenced` re-checks the root set and
/// refuses regardless, so this plan is advisory: two independent checks
/// against the same root set, which is what keeps a stale plan from deleting
/// a generation that became referenced between planning and sweeping.
pub fn plan_history_gc(
    store: &HistoryGenerationStore,
    roots: &BTreeSet<String>,
) -> anyhow::Result<Vec<HistoryGenerationIdV1>> {
    Ok(store
        .list()
        .map_err(|error| anyhow::anyhow!("{error}"))?
        .into_iter()
        .filter(|id| !roots.contains(id.as_str()))
        .collect())
}

/// Read the durable overlay selectors the reference rebuild consumes.
pub fn selected_overlays_for_gc(
    edges_dir: &Path,
) -> anyhow::Result<BTreeMap<String, GitOverlaySelector>> {
    bbox_edge_sidecar::snapshot::selected_git_overlays(edges_dir)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        AmbiguousNamespaceRecord, AmbiguousNamespaceStatus, CommitNamespace, RecordedRepoAuthority,
        RepoHistoryAuthority, RepoHistoryGenerationId, RepoHistoryId,
        RepoHistoryQuarantineGenerationId, RepoHistoryRecord,
    };

    fn generation_id(seed: char) -> String {
        format!("rhg_{}", std::iter::repeat_n(seed, 64).collect::<String>())
    }

    fn quarantine_id(seed: char) -> String {
        format!("rhq_{}", std::iter::repeat_n(seed, 64).collect::<String>())
    }

    fn overlay(project: &str, generation: &str) -> GitOverlaySelector {
        GitOverlaySelector {
            project_id: project.to_string(),
            code_generation: "code-gen".to_string(),
            repo_history_generation: generation.to_string(),
            source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
                attachment_id: "att_1".to_string(),
            },
            repo_head: "b".repeat(40),
            commit_namespace: "nsmono".to_string(),
            overlay_generation: 1,
        }
    }

    fn catalog_with_ready_history(generation: &str) -> CatalogSnapshotV2 {
        let mut catalog = CatalogSnapshotV2::empty(7).unwrap();
        let id = RepoHistoryId::parse(format!("rh_{:0>32}", "1")).unwrap();
        catalog.repo_histories.insert(
            id.clone(),
            RepoHistoryRecord {
                repo_history_id: id,
                membership_generation: 0,
                authority: RepoHistoryAuthority::Recorded(
                    RecordedRepoAuthority::parse("repo-authority".to_string()).unwrap(),
                ),
                primary_namespace: CommitNamespace::parse("nsmono".to_string()).unwrap(),
                compatibility_namespaces: Default::default(),
                materialization: RepoHistoryMaterialization::Ready {
                    generation_id: RepoHistoryGenerationId::parse(generation.to_string()).unwrap(),
                },
            },
        );
        catalog
    }

    #[test]
    fn references_fold_catalog_quarantine_and_overlay_inputs() {
        let owned = generation_id('a');
        let quarantined = quarantine_id('b');
        let overlay_generation = generation_id('c');
        let mut catalog = catalog_with_ready_history(&owned);
        let namespace = CommitNamespace::parse("nsquar".to_string()).unwrap();
        catalog.ambiguous_namespaces.insert(
            namespace.clone(),
            AmbiguousNamespaceRecord {
                namespace,
                candidate_repo_history_ids: Default::default(),
                status: AmbiguousNamespaceStatus::Quarantined,
                materialization: RepoHistoryQuarantineMaterialization::Ready {
                    generation_id: RepoHistoryQuarantineGenerationId::parse(quarantined.clone())
                        .unwrap(),
                },
            },
        );
        let overlays = BTreeMap::from([("p_1".to_string(), overlay("p_1", &overlay_generation))]);
        let manifest =
            build_reference_manifest(&catalog, &overlays, &[], &BTreeSet::new(), &BTreeSet::new());
        assert_eq!(manifest.roots().len(), 3);
        assert!(manifest.references[&owned].contains(&HistoryReferenceKindV1::CatalogRecord));
        assert!(
            manifest.references[&quarantined].contains(&HistoryReferenceKindV1::QuarantineRecord)
        );
        assert!(
            manifest.references[&overlay_generation]
                .contains(&HistoryReferenceKindV1::ActiveOverlay)
        );
    }

    #[test]
    fn retiring_one_sibling_keeps_shared_history_referenced() {
        // Two monorepo siblings share ONE repo-history generation through
        // their overlays. Retiring one removes only that project's overlay.
        let shared = generation_id('a');
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let both = BTreeMap::from([
            ("p_alpha".to_string(), overlay("p_alpha", &shared)),
            ("p_beta".to_string(), overlay("p_beta", &shared)),
        ]);
        let before =
            build_reference_manifest(&catalog, &both, &[], &BTreeSet::new(), &BTreeSet::new());
        assert!(before.roots().contains(&shared));

        let mut after_retire = both.clone();
        after_retire.remove("p_alpha");
        let after = build_reference_manifest(
            &catalog,
            &after_retire,
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        assert!(
            after.roots().contains(&shared),
            "the surviving sibling's overlay still references the shared generation"
        );

        let detached = BTreeMap::new();
        let none =
            build_reference_manifest(&catalog, &detached, &[], &BTreeSet::new(), &BTreeSet::new());
        assert!(
            !none.roots().contains(&shared),
            "with no catalog record and no overlay left, nothing references it"
        );
    }

    #[test]
    fn process_local_roots_are_roots_but_do_not_move_the_checksum() {
        let catalog = CatalogSnapshotV2::empty(1).unwrap();
        let pinned = generation_id('d');
        let bare = build_reference_manifest(
            &catalog,
            &BTreeMap::new(),
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        let with_view = build_reference_manifest(
            &catalog,
            &BTreeMap::new(),
            &[],
            &BTreeSet::from([pinned.clone()]),
            &BTreeSet::new(),
        );
        assert!(with_view.roots().contains(&pinned));
        assert_eq!(
            bare.checksum_sha256, with_view.checksum_sha256,
            "a pinned read view cannot survive a restart, so folding it into the \
             durable checksum would make every restart look like drift"
        );
    }

    /// Fixture: a generations root with a persisted manifest already
    /// baselined from `catalog`, plus a handle to the store.
    fn baselined_store(
        directory: &tempfile::TempDir,
        catalog: &CatalogSnapshotV2,
    ) -> (HistoryGenerationStore, HistoryReferenceManifestV1) {
        let index_path = directory.path().canonicalize().unwrap().join("index");
        fs::create_dir_all(&index_path).unwrap();
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let baseline = build_reference_manifest(
            catalog,
            &BTreeMap::new(),
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        match evaluate_history_gc(&store, &baseline) {
            HistoryGcEnablementV1::Enabled { divergence, .. } => {
                assert!(
                    divergence.is_none(),
                    "a first baseline diverges from nothing"
                );
            }
            other => panic!("a missing manifest must baseline, not refuse: {other:?}"),
        }
        (store, baseline)
    }

    #[test]
    fn a_missing_manifest_baselines_and_a_matching_one_stays_quiet() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_with_ready_history(&generation_id('a'));
        let (store, baseline) = baselined_store(&directory, &catalog);
        assert!(
            read_reference_manifest(&store).unwrap().is_some(),
            "baselining must persist, or the next pass baselines again forever"
        );
        match evaluate_history_gc(&store, &baseline) {
            HistoryGcEnablementV1::Enabled { roots, divergence } => {
                assert_eq!(roots.len(), 1);
                assert!(
                    divergence.is_none(),
                    "identical inputs are not a divergence"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// D-038: the accept-and-persist path. An ordinary overlay swap changes a
    /// durable input, and NO sanctioned mutation path writes the acceleration
    /// index, so the very next evaluation legitimately disagrees with the
    /// persisted bytes.
    #[test]
    fn an_ordinary_swap_diverges_once_is_accepted_and_persisted_then_stays_quiet() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_with_ready_history(&generation_id('a'));
        let (store, baseline) = baselined_store(&directory, &catalog);

        let swapped = generation_id('e');
        let overlays = BTreeMap::from([("p_1".to_string(), overlay("p_1", &swapped))]);
        let after_swap =
            build_reference_manifest(&catalog, &overlays, &[], &BTreeSet::new(), &BTreeSet::new());
        assert_ne!(baseline.checksum_sha256, after_swap.checksum_sha256);

        let divergence = match evaluate_history_gc(&store, &after_swap) {
            HistoryGcEnablementV1::Enabled { roots, divergence } => {
                assert!(
                    roots.contains(&swapped),
                    "the swapped-in generation is a root the moment the overlay is durable"
                );
                divergence.expect("a stale persisted index must be reported")
            }
            other => panic!("a stale index must not disable GC: {other:?}"),
        };
        assert_eq!(
            divergence.persisted_checksum_sha256,
            baseline.checksum_sha256
        );
        assert_eq!(
            divergence.rebuilt_checksum_sha256,
            after_swap.checksum_sha256
        );
        assert!(divergence.note.contains("stale"), "{}", divergence.note);
        assert_eq!(
            read_reference_manifest(&store)
                .unwrap()
                .unwrap()
                .checksum_sha256,
            after_swap.checksum_sha256,
            "accept means PERSIST; without this the next pass re-derives the same \
             divergence forever"
        );

        // The latch regression itself: a second evaluation against the same
        // inputs is quiet and still enabled.
        match evaluate_history_gc(&store, &after_swap) {
            HistoryGcEnablementV1::Enabled { roots, divergence } => {
                assert!(roots.contains(&swapped));
                assert!(
                    divergence.is_none(),
                    "the first cut latched here: mismatch without persist re-derived \
                     the same mismatch on every later pass"
                );
            }
            other => panic!("{other:?}"),
        }
    }

    /// A version difference is a divergence, not a refusal: differing versions
    /// mean the two checksums were computed by different rules and are not
    /// comparable, which the rebuild resolves the same way staleness is.
    #[test]
    fn a_foreign_manifest_version_is_accepted_as_divergence() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_with_ready_history(&generation_id('a'));
        let (store, baseline) = baselined_store(&directory, &catalog);

        let mut foreign = read_reference_manifest(&store).unwrap().unwrap();
        foreign.version = REFERENCE_MANIFEST_VERSION_V1 + 7;
        write_reference_manifest(&store, &foreign).unwrap();

        match evaluate_history_gc(&store, &baseline) {
            HistoryGcEnablementV1::Enabled { divergence, .. } => {
                let divergence = divergence.expect("a foreign version must be reported");
                assert_eq!(
                    divergence.persisted_version,
                    REFERENCE_MANIFEST_VERSION_V1 + 7
                );
                assert_eq!(divergence.rebuilt_version, REFERENCE_MANIFEST_VERSION_V1);
            }
            other => panic!("a foreign version must not disable GC: {other:?}"),
        }
        assert_eq!(
            read_reference_manifest(&store).unwrap().unwrap().version,
            REFERENCE_MANIFEST_VERSION_V1
        );
    }

    /// Surviving `Disabled` arm 1: the persisted file cannot be reached at
    /// all. A directory in its place is the portable way to make `fs::read`
    /// fail with something other than NotFound.
    #[test]
    fn an_unreadable_persisted_manifest_disables_gc() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_with_ready_history(&generation_id('a'));
        let (store, baseline) = baselined_store(&directory, &catalog);

        let path = store.root().join(REFERENCE_MANIFEST_FILE);
        fs::remove_file(&path).unwrap();
        fs::create_dir_all(&path).unwrap();

        match evaluate_history_gc(&store, &baseline) {
            HistoryGcEnablementV1::Disabled { diagnostic } => {
                assert!(
                    diagnostic.contains("could not be read or decoded"),
                    "{diagnostic}"
                );
            }
            other => panic!("an unreachable manifest must disable GC: {other:?}"),
        }
    }

    /// Surviving `Disabled` arm 2: the file exists and is reachable but its
    /// bytes are not a manifest. Unlike staleness, this is unexplained
    /// corruption of bytes this daemon wrote, and the honest answer is that
    /// the evaluation could not be performed.
    #[test]
    fn an_undecodable_persisted_manifest_disables_gc() {
        let directory = tempfile::tempdir().unwrap();
        let catalog = catalog_with_ready_history(&generation_id('a'));
        let (store, baseline) = baselined_store(&directory, &catalog);

        fs::write(
            store.root().join(REFERENCE_MANIFEST_FILE),
            b"{not a reference manifest",
        )
        .unwrap();

        match evaluate_history_gc(&store, &baseline) {
            HistoryGcEnablementV1::Disabled { diagnostic } => {
                assert!(
                    diagnostic.contains("could not be read or decoded"),
                    "{diagnostic}"
                );
            }
            other => panic!("undecodable bytes must disable GC: {other:?}"),
        }
    }

    #[test]
    fn a_crash_between_overlay_swap_and_manifest_refresh_cannot_free_the_generation() {
        // Simulate: the overlay swap landed durably (the manifest index has
        // the selector) but the process died before refreshing the reference
        // manifest, so the persisted one predates the swap.
        let directory = tempfile::tempdir().unwrap();
        let index_path = directory.path().canonicalize().unwrap().join("index");
        fs::create_dir_all(&index_path).unwrap();
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let catalog = CatalogSnapshotV2::empty(1).unwrap();

        let pre_swap = build_reference_manifest(
            &catalog,
            &BTreeMap::new(),
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        write_reference_manifest(&store, &pre_swap).unwrap();

        let swapped = generation_id('e');
        let overlays = BTreeMap::from([("p_1".to_string(), overlay("p_1", &swapped))]);
        let rebuilt =
            build_reference_manifest(&catalog, &overlays, &[], &BTreeSet::new(), &BTreeSet::new());

        // The safety claim, unchanged: nothing could have freed the swapped-in
        // generation in the crash window, because the rebuild reads the
        // durable overlay selector rather than the stale persisted index.
        assert!(
            rebuilt.roots().contains(&swapped),
            "the rebuild reads the durable overlay selector, so the swapped-in \
             generation is a root even though the persisted manifest predates it"
        );
        assert!(
            !pre_swap.roots().contains(&swapped),
            "the persisted index genuinely predates the swap, so this is the real window"
        );

        // D-038: and recovery CONVERGES rather than latching. The evaluation
        // accepts the rebuild, persists it, and leaves GC enabled with the
        // generation still rooted.
        match evaluate_history_gc(&store, &rebuilt) {
            HistoryGcEnablementV1::Enabled { roots, divergence } => {
                assert!(roots.contains(&swapped));
                assert!(
                    divergence.is_some(),
                    "the crash window is a real divergence"
                );
            }
            other => panic!("crash recovery must converge, not latch off: {other:?}"),
        }
        assert_eq!(
            read_reference_manifest(&store)
                .unwrap()
                .unwrap()
                .checksum_sha256,
            rebuilt.checksum_sha256
        );
        assert!(matches!(
            evaluate_history_gc(&store, &rebuilt),
            HistoryGcEnablementV1::Enabled {
                divergence: None,
                ..
            }
        ));
    }

    struct RecordingSink(std::cell::RefCell<Vec<Vec<String>>>);

    impl HistoryVectorTombstoneSink for RecordingSink {
        fn delete_entities_all_routes(&self, entity_ids: &[String]) -> anyhow::Result<()> {
            self.0.borrow_mut().push(entity_ids.to_vec());
            Ok(())
        }
    }

    #[test]
    fn vector_tombstones_iterate_the_generations_own_inventory() {
        use bbox_corpus_index::index::history_generations::{
            HistoryGenerationInputV1, HistoryGenerationOwnerV1, generation_rows_for_commit,
        };

        let directory = tempfile::tempdir().unwrap();
        let index_path = directory.path().canonicalize().unwrap().join("index");
        fs::create_dir_all(&index_path).unwrap();
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let mut documents = Vec::new();
        let mut inputs = Vec::new();
        for seed in ['1', '2', '3'] {
            let commit = bbox_corpus_core::git::GitCommit {
                sha: std::iter::repeat_n(seed, 40).collect(),
                parent_shas: Vec::new(),
                author_name: "A".into(),
                author_email: "a@example.test".into(),
                message: format!("commit {seed}"),
            };
            let (document, input) = generation_rows_for_commit(&commit, "nsmono");
            documents.push(document);
            inputs.push(input);
        }
        let generation = store
            .create_or_open(HistoryGenerationInputV1 {
                namespace: CommitNamespace::parse("nsmono".to_string()).unwrap(),
                owner: HistoryGenerationOwnerV1::Owned {
                    repo_history_id: RepoHistoryId::parse(format!("rh_{:0>32}", "1")).unwrap(),
                },
                commit_documents: documents,
                vector_inputs: inputs,
                truncated_message_count: 0,
                source_schema_version: "v".into(),
                source_schema_fingerprint_sha256: "f".into(),
                source_index_fingerprint_sha256: "s".into(),
            })
            .unwrap();

        let sink = RecordingSink(Default::default());
        let deleted = tombstone_generation_vectors(&generation, &sink).unwrap();
        assert_eq!(deleted, 3);
        let recorded = sink.0.borrow();
        assert_eq!(recorded.len(), 1, "history GC must issue one batch call");
        assert_eq!(recorded[0].len(), 3);
        assert!(
            recorded[0]
                .iter()
                .all(|id| id.starts_with("commit:nsmono:")),
            "commit vectors are repo-scoped; a project code selector could not \
             have produced this set"
        );
    }

    #[test]
    fn gc_plan_excludes_every_root() {
        let directory = tempfile::tempdir().unwrap();
        let index_path = directory.path().canonicalize().unwrap().join("index");
        fs::create_dir_all(&index_path).unwrap();
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();
        let planned = plan_history_gc(&store, &BTreeSet::new()).unwrap();
        assert!(planned.is_empty(), "an empty store plans nothing");
    }
}

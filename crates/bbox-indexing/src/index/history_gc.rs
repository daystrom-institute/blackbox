//! History generation GC: the derived reference manifest and the
//! generation-driven vector tombstone path (Phase 3 plan section 10 item 4;
//! governing section 11 and section 16).
//!
//! THE MANIFEST IS AN ACCELERATION INDEX, NOT AUTHORITY. Its durable inputs
//! are the persisted catalog records and the active/retained Git overlay
//! selectors in the edge sidecar's manifest index. It is rebuilt and
//! checksummed from those inputs at startup and before EVERY GC pass; a
//! mismatch disables history GC and reports a doctor finding, and never hides
//! an otherwise-valid history read. That asymmetry is the design: a stale
//! acceleration index may cost a sweep, but it must never cost a generation.
//!
//! WHY A CRASH BETWEEN AN OVERLAY SWAP AND A MANIFEST REFRESH IS SAFE. The
//! overlay selector is written atomically into the workspace manifest entry
//! inside the manifest coordinator, and that entry is a DURABLE INPUT to this
//! rebuild rather than something the reference manifest caches independently.
//! So a process that dies after the swap and before any refresh still
//! recomputes a reference set containing the swapped-in generation the next
//! time it starts. There is no window in which the overlay exists on disk but
//! the generation looks unreferenced.
//!
//! IN-PROCESS ROOTS. Pinned read views and in-flight builds are added to the
//! rebuilt durable set while the process runs. They cannot be persisted (they
//! do not survive the process that holds them) and they do not need to be: a
//! restart cannot be holding them.

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

/// Whether history GC may run this pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryGcEnablementV1 {
    Enabled {
        roots: BTreeSet<String>,
    },
    /// GC is off for this pass. `diagnostic` is doctor-facing prose.
    Disabled {
        diagnostic: String,
    },
}

/// Rebuild the manifest, compare it against the persisted one, and decide
/// whether history GC may run.
///
/// A MISSING persisted manifest is not a mismatch: it is the first pass on a
/// store that predates this field, and the rebuild becomes the baseline. A
/// PRESENT manifest whose durable checksum disagrees IS a mismatch: something
/// mutated a durable input without going through the paths that refresh this
/// index, and sweeping under that uncertainty is exactly what governing
/// section 11 forbids.
pub fn evaluate_history_gc(
    store: &HistoryGenerationStore,
    rebuilt: &HistoryReferenceManifestV1,
) -> HistoryGcEnablementV1 {
    let persisted = match read_reference_manifest(store) {
        Ok(persisted) => persisted,
        Err(error) => {
            return HistoryGcEnablementV1::Disabled {
                diagnostic: format!(
                    "the repo-history reference manifest is unreadable ({error}); \
                     history GC is disabled until it is rebuilt"
                ),
            };
        }
    };
    let Some(persisted) = persisted else {
        if let Err(error) = write_reference_manifest(store, rebuilt) {
            return HistoryGcEnablementV1::Disabled {
                diagnostic: format!(
                    "the repo-history reference manifest could not be written ({error}); \
                     history GC is disabled"
                ),
            };
        }
        return HistoryGcEnablementV1::Enabled {
            roots: rebuilt.roots(),
        };
    };
    if persisted.version != rebuilt.version {
        return HistoryGcEnablementV1::Disabled {
            diagnostic: format!(
                "the repo-history reference manifest is version {} but this daemon \
                 builds version {}; history GC is disabled",
                persisted.version, rebuilt.version
            ),
        };
    }
    // The epoch is deliberately NOT part of the comparison: a catalog
    // mutation that changes no history reference legitimately bumps it, and
    // treating that as drift would disable GC on every unrelated write.
    if persisted.checksum_sha256 != rebuilt.checksum_sha256 {
        return HistoryGcEnablementV1::Disabled {
            diagnostic: format!(
                "the repo-history reference manifest checksum does not re-derive \
                 (persisted {}, rebuilt {}); history GC is disabled until the \
                 divergence is explained",
                &persisted.checksum_sha256[..persisted.checksum_sha256.len().min(12)],
                &rebuilt.checksum_sha256[..rebuilt.checksum_sha256.len().min(12)],
            ),
        };
    }
    if let Err(error) = write_reference_manifest(store, rebuilt) {
        return HistoryGcEnablementV1::Disabled {
            diagnostic: format!(
                "the repo-history reference manifest could not be refreshed ({error}); \
                 history GC is disabled"
            ),
        };
    }
    HistoryGcEnablementV1::Enabled {
        roots: rebuilt.roots(),
    }
}

/// A sink for vector tombstones, so the sweep is testable without a live
/// vector store and so the single-writer discipline stays at the call site.
pub trait HistoryVectorTombstoneSink {
    fn delete_entity_all_routes(&self, entity_id: &str) -> anyhow::Result<()>;
}

/// Tombstone the vectors of ONE retired history generation.
///
/// Iterates the generation's OWN vector-input inventory, one
/// `delete_entity_all_routes` per entity. It never derives the entity set
/// from a project code selector: commit vectors are repo-scoped and enqueued
/// once per repository, so a project-selector-driven delete would either miss
/// them entirely (they carry no project) or, for a monorepo, delete a
/// sibling's live commit vectors on the first project's retirement.
pub fn tombstone_generation_vectors(
    generation: &HistoryGenerationRecordV1,
    sink: &dyn HistoryVectorTombstoneSink,
) -> anyhow::Result<u64> {
    let mut deleted = 0_u64;
    for input in &generation.vector_inputs {
        sink.delete_entity_all_routes(&input.entity_id)?;
        deleted += 1;
    }
    Ok(deleted)
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
            attachment_id: "att_1".to_string(),
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

    #[test]
    fn checksum_mismatch_disables_gc_and_a_missing_manifest_does_not() {
        let directory = tempfile::tempdir().unwrap();
        let index_path = directory.path().canonicalize().unwrap().join("index");
        fs::create_dir_all(&index_path).unwrap();
        let store = HistoryGenerationStore::open_for_index(&index_path).unwrap();

        let first = build_reference_manifest(
            &catalog_with_ready_history(&generation_id('a')),
            &BTreeMap::new(),
            &[],
            &BTreeSet::new(),
            &BTreeSet::new(),
        );
        match evaluate_history_gc(&store, &first) {
            HistoryGcEnablementV1::Enabled { roots } => assert_eq!(roots.len(), 1),
            other => panic!("a missing manifest must baseline, not refuse: {other:?}"),
        }
        // Same inputs re-derive the same checksum: GC stays enabled.
        assert!(matches!(
            evaluate_history_gc(&store, &first),
            HistoryGcEnablementV1::Enabled { .. }
        ));

        // Corrupt the persisted manifest's checksum out of band.
        let mut tampered = read_reference_manifest(&store).unwrap().unwrap();
        tampered.checksum_sha256 = "0".repeat(64);
        write_reference_manifest(&store, &tampered).unwrap();
        match evaluate_history_gc(&store, &first) {
            HistoryGcEnablementV1::Disabled { diagnostic } => {
                assert!(diagnostic.contains("does not re-derive"), "{diagnostic}");
            }
            other => panic!("a checksum mismatch must disable GC: {other:?}"),
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
        assert!(
            rebuilt.roots().contains(&swapped),
            "the rebuild reads the durable overlay selector, so the swapped-in \
             generation is a root even though the persisted manifest predates it"
        );
        // And the divergence is loud rather than silent: GC is off until it
        // is explained, which is strictly safer than sweeping.
        assert!(matches!(
            evaluate_history_gc(&store, &rebuilt),
            HistoryGcEnablementV1::Disabled { .. }
        ));
    }

    struct RecordingSink(std::cell::RefCell<Vec<String>>);

    impl HistoryVectorTombstoneSink for RecordingSink {
        fn delete_entity_all_routes(&self, entity_id: &str) -> anyhow::Result<()> {
            self.0.borrow_mut().push(entity_id.to_string());
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
        assert_eq!(recorded.len(), 3);
        assert!(
            recorded.iter().all(|id| id.starts_with("commit:nsmono:")),
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

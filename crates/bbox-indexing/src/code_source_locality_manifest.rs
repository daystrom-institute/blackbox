//! Classify, and where it is provably safe repair, a disagreement between the
//! code-source activation journal and the derived edge-sidecar workspace
//! manifest at daemon open.
//!
//! # The two stores and their write ordering
//!
//! A collected activation (`activate_desired_loop`) touches two durable stores
//! that share no transaction, in this literal order:
//!
//! 1. `record_materialization_mixed` - journal: the generation becomes
//!    materialized (doc count and entity inventory installed).
//! 2. `save_activation_v2` - journal: the activation record for the new
//!    generation is durably installed.
//! 3. `activate_collected_snapshot` - manifest: one atomic replacement
//!    publishes `code_source_selector`, `code_source_generation`, and
//!    `active_snapshot`.
//! 4. `mark_generation_state_mixed(.., Active)` - journal: the generation's
//!    state flips to `Active`.
//!
//! Each store is individually crash safe (fsync, atomic rename, parent fsync);
//! the pair is not, and the two mutual-exclusion locks are different locks
//! neither of which is held across the other. So every boundary above is an
//! observable crash window, and the generation's state is what tells the two
//! interesting ones apart:
//!
//! - Crash between 2 and 3: the journal names generation N, the manifest still
//!   names its predecessor, and N is **not** `Active`. The activation did not
//!   complete. The manifest is RIGHT: the project is still correctly serving
//!   the predecessor, and the startup reducer sweep converges the in-flight
//!   record. Nothing here rewrites the manifest for this shape.
//! - Crash between 3 and 4: journal and manifest agree on N, but N is not
//!   `Active`. That is the completed activation whose state flip was lost, and
//!   `code_source_locality_cutover` already tolerates it on exactly that
//!   agreement.
//!
//! Because the state flip is written LAST, an `Active` generation is proof
//! that its manifest write was issued and returned. An `Active` generation
//! whose manifest entry is absent or names an older collected generation is
//! therefore a lost derived write, not an in-flight activation, and the
//! journal is the authority that repairs it.
//!
//! # The inverse ordering, and why the journal does not always win
//!
//! `cutback_to_local` writes the two stores in the OPPOSITE order: the
//! manifest is published local first, then the journal's generation state and
//! activation record are cleared. Its crash window leaves a `local:<project>`
//! manifest entry beside a collected activation record, and the sanctioned
//! convergence there clears the stale JOURNAL record: the manifest wins. That
//! shape is recognized and deliberately left alone here, so this module never
//! fights the relationship chain's crash-window admission
//! (`is_cutback_crash_window`).
//!
//! Every interleaving therefore has a defined outcome, and none of them is a
//! crash loop.
//!
//! # Three readers, two stores
//!
//! The relationship chain (`validate_relationship_chain`, link 5) is NOT a
//! third store. It is a third READER that re-derives the same journal-versus-
//! manifest comparison independently, alongside this module's boot evidence
//! loop and the locality cutover's live re-verification. All three compare the
//! same two durable owners, so a tear presents to all three, and a tolerance
//! rule taught to only one of them just moves the crash loop downstream.
//!
//! That makes ORDER a correctness property of the boot sequence, not a
//! detail. `reconcile_workspace_manifests_from_activations` runs in the
//! pre-bind recovery BEFORE the chain validates, so the chain sees reconciled
//! state rather than the tear. The chain itself keeps its strict contract for
//! every other caller: it is a pure validator that neither repairs nor is
//! relaxed for the shape the journal owns. Its only added tolerance is the
//! shape nothing can repair yet, an activation still in flight, which it
//! admits exactly as it already admits an absent entry and the cutback crash
//! window, deferring to the startup reducer sweep.

use std::path::Path;

use anyhow::{Context, Result};
use bbox_code_source::GenerationState;
use bbox_code_source_store::{
    ActivationRecordV2, CodeSourceStore, MixedActivationRecord, MixedStoredGeneration,
};
use bbox_edge_sidecar::manifest::{ManifestIndex, WorkspaceIndexEntry};

/// What the derived manifest entry looks like against the journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceManifestState {
    /// The entry carries exactly what the journal says is active.
    Agreed,
    /// The entry was absent or named an older collected generation while the
    /// journal's generation is `Active`; it was repaired from the journal.
    /// Carries the generation the entry named before the repair, if any.
    Reconciled { previous_generation: Option<String> },
    /// The entry names a valid predecessor collected generation and the
    /// journal's generation is not `Active`: an activation torn between its
    /// journal write and its manifest write. The manifest is correct and the
    /// reducer sweep converges the record; boot callers pass over the project
    /// rather than repairing or refusing.
    ActivationInFlight { journal_generation: String },
    /// Anything else, including the cutback crash window and any entry shape
    /// this module does not recognize. Callers refuse exactly as they did
    /// before this module existed.
    Disagrees,
}

/// Does the derived manifest entry already carry exactly what the journal says
/// is active?
///
/// This is the three-way comparison every collected-source boot path makes:
/// selector, generation id, and the active snapshot relative path.
pub fn manifest_entry_matches_activation(
    entry: &WorkspaceIndexEntry,
    activation: &ActivationRecordV2,
    expected_snapshot: &str,
) -> bool {
    entry.code_source_selector.as_deref() == Some(activation.selector.as_str())
        && entry.code_source_generation.as_deref() == Some(activation.generation_id.as_str())
        && entry.active_snapshot.as_deref() == Some(expected_snapshot)
}

/// Classify the manifest entry for one current collected activation, and
/// repair it when the journal is provably the completed authority.
///
/// `generation_state` is the state of the generation the activation record
/// names, which the caller has already loaded and proven with
/// `validate_against_generation`. That validation is what makes the record's
/// selector, generation id, and snapshot id trustworthy inputs to a repair,
/// and it is unchanged by this module: an activation naming a generation the
/// store cannot produce, or one that fails to validate, still refuses before
/// reaching here.
///
/// # Caller constraint
///
/// A repair takes the process-wide manifest coordinator, a plain non-reentrant
/// mutex. Never call this while already holding that coordinator.
pub fn classify_workspace_manifest(
    edges_dir: &Path,
    activation: &ActivationRecordV2,
    generation_state: GenerationState,
) -> Result<WorkspaceManifestState> {
    let project_id = activation.project_id.as_str();
    let expected_snapshot =
        bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, &activation.snapshot_id);
    let index = ManifestIndex::load_or_new(edges_dir)
        .context("loading the workspace manifest for code-source classification")?;
    let entry = index.workspaces.get(project_id);

    if entry.is_some_and(|entry| {
        manifest_entry_matches_activation(entry, activation, &expected_snapshot)
    }) {
        return Ok(WorkspaceManifestState::Agreed);
    }
    // This module owns collected activations only. A local activation's
    // manifest entry is published by a different writer.
    if !activation.selector.starts_with("collected:") {
        return Ok(WorkspaceManifestState::Disagrees);
    }

    let previous_generation = match entry {
        None => None,
        Some(entry) => {
            // The cutback crash window: the local writer published its entry
            // and the crash landed before the journal record was cleared.
            // The manifest wins there, and the relationship chain admits it
            // on its own terms. Not ours to touch.
            if entry.code_source_selector.as_deref()
                == Some(bbox_code_source::local_selector(project_id).as_str())
            {
                return Ok(WorkspaceManifestState::Disagrees);
            }
            // Only a well-formed predecessor collected entry is a recognized
            // torn shape. An entry that is malformed, cross-project,
            // traversal bearing, or disagreeing on something the atomic
            // manifest write publishes together is drift, and drift still
            // refuses.
            if !is_torn_activation_predecessor(
                entry,
                project_id,
                &activation.selector,
                &activation.generation_id,
            ) {
                return Ok(WorkspaceManifestState::Disagrees);
            }
            entry.code_source_generation.clone()
        }
    };

    if generation_state != GenerationState::Active {
        // The state flip is written after the manifest write, so a
        // non-Active generation whose manifest names the predecessor is an
        // activation still in flight. The manifest is right; leave it.
        return Ok(WorkspaceManifestState::ActivationInFlight {
            journal_generation: activation.generation_id.clone(),
        });
    }

    reconcile_workspace_manifest_entry(edges_dir, activation, &expected_snapshot)?;
    tracing::warn!(
        project_id = %project_id,
        journal_generation = %activation.generation_id,
        manifest_generation = previous_generation.as_deref().unwrap_or("<absent>"),
        "reconciled a workspace manifest entry that a torn activation left behind the code-source activation journal"
    );
    Ok(WorkspaceManifestState::Reconciled {
        previous_generation,
    })
}

/// Does this entry look like the collected writer's own publication for THIS
/// project, naming an EARLIER generation than the one the journal names?
///
/// That is the signature of an activation torn between its journal write and
/// its manifest write, and it is the only present-entry shape either the
/// classifier or the relationship chain may treat as a crash window rather
/// than drift. The shape validation mirrors the rigor the chain already
/// applies to its cutback crash-window admission: exact manifest path for this
/// project, a collected selector, and a same-project confined snapshot path
/// with no traversal.
///
/// Requiring the entry to name a DIFFERENT generation is what keeps this from
/// laundering drift. The manifest write publishes all three collected fields
/// in one atomic replacement, so an entry that agrees on the generation but
/// disagrees on the selector or the snapshot cannot have come from a torn
/// write, and stays a refusal.
pub fn is_torn_activation_predecessor(
    entry: &WorkspaceIndexEntry,
    project_id: &str,
    activation_selector: &str,
    activation_generation: &str,
) -> bool {
    if !activation_selector.starts_with("collected:") {
        return false;
    }
    let Some(selector) = entry.code_source_selector.as_deref() else {
        return false;
    };
    if !selector.starts_with("collected:") || selector == activation_selector {
        return false;
    }
    match entry.code_source_generation.as_deref() {
        Some(generation) if generation != activation_generation => {}
        _ => return false,
    }
    if entry.manifest != format!("workspace/{project_id}/manifest.json") {
        return false;
    }
    let Some(snapshot) = entry.active_snapshot.as_deref() else {
        return false;
    };
    snapshot.starts_with(&format!("workspace/{project_id}/snapshots/"))
        && !snapshot.contains("..")
        && !snapshot.contains('\0')
}

/// Repair every workspace manifest entry a torn activation left behind the
/// journal, and report how many were repaired.
///
/// This is the sweep the pre-bind recovery runs BEFORE it validates the
/// relationship chain, so the chain validates reconciled state rather than the
/// tear. It is deliberately permissive: any activation it cannot fully prove
/// (a generation the store cannot produce, a legacy record, a
/// `validate_against_generation` failure) is skipped rather than refused here,
/// so the chain still emits its own precise refusal for that record instead of
/// this sweep masking it with a different error.
pub fn reconcile_workspace_manifests_from_activations(
    store: &CodeSourceStore,
    projects_path: &Path,
) -> Result<usize> {
    let edges_dir = bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(projects_path);
    let mut reconciled = 0;
    for activation in store.activation_records_mixed()? {
        let MixedActivationRecord::CurrentV2(activation) = activation else {
            continue;
        };
        if activation.cutback.is_some() || activation.cutback_pending {
            continue;
        }
        let Ok(generation) = store.find_generation_mixed(&activation.generation_id) else {
            continue;
        };
        let MixedStoredGeneration::CurrentV2(generation) = generation else {
            continue;
        };
        if activation.validate_against_generation(&generation).is_err() {
            continue;
        }
        if matches!(
            classify_workspace_manifest(&edges_dir, &activation, generation.state)?,
            WorkspaceManifestState::Reconciled { .. }
        ) {
            reconciled += 1;
        }
    }
    Ok(reconciled)
}

/// Rewrite the derived entry from the activation record.
///
/// This is a field upsert rather than a replay of `activate_collected_snapshot`
/// on purpose: the replay additionally requires the snapshot's staged edge
/// directory to be present, and boot deliberately tolerates a collected
/// workspace whose materialization is fully absent pending its first
/// republish, so requiring staged edges here would convert a tolerated state
/// into a refusal. It mirrors the field-level reconstruction pre-bind recovery
/// already performs for absent entries and extends it to the stale case, which
/// nothing covered before.
///
/// `repo_materialization` is carried over exactly as the production writer
/// does. `git_overlay` is cleared and `git_overlay_managed` set, also matching
/// the production writer: an overlay is bound to one code generation, so an
/// overlay selected for the superseded generation must not survive the swap.
fn reconcile_workspace_manifest_entry(
    edges_dir: &Path,
    activation: &ActivationRecordV2,
    expected_snapshot: &str,
) -> Result<()> {
    let project_id = activation.project_id.as_str();
    bbox_edge_sidecar::snapshot::with_manifest_coordinator(|| {
        let mut index = ManifestIndex::load_or_new(edges_dir)
            .context("loading the workspace manifest for code-source reconciliation")?;
        let repo_materialization = index
            .workspaces
            .get(project_id)
            .and_then(|entry| entry.repo_materialization.clone());
        index.upsert_workspace(
            project_id,
            WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(expected_snapshot.to_string()),
                dirty_overlay: None,
                repo_materialization,
                code_source_selector: Some(activation.selector.clone()),
                code_source_generation: Some(activation.generation_id.clone()),
                git_overlay: None,
                git_overlay_managed: true,
            },
        );
        index
            .write_atomic(edges_dir)
            .context("writing the reconciled workspace manifest entry")
    })?;

    let index = ManifestIndex::load_or_new(edges_dir)
        .context("re-reading the workspace manifest after code-source reconciliation")?;
    let entry = index
        .workspaces
        .get(project_id)
        .context("reconciled collected generation is still absent from the workspace manifest")?;
    if !manifest_entry_matches_activation(entry, activation, expected_snapshot) {
        anyhow::bail!(
            "active collected generation disagrees with the workspace manifest after reconciling"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::ProjectId;

    const PROJECT: &str = "p_00000000000000000000000000000001";

    fn project_id() -> ProjectId {
        ProjectId::parse(PROJECT).unwrap()
    }

    /// The journal record for the generation the daemon believes is current.
    fn activation(generation_id: &str) -> ActivationRecordV2 {
        ActivationRecordV2 {
            version: bbox_code_source_store::MIGRATION_STORE_VERSION,
            project_id: project_id(),
            published_scope: PublishedScope::try_new("fixture-repo", ".").unwrap(),
            generation_id: generation_id.to_string(),
            selector: format!("collected:{PROJECT}:{generation_id}"),
            snapshot_id: format!("collected-{generation_id}"),
            document_count: 0,
            entity_inventory_sha256: "b".repeat(64),
            current_chunk_targets: BTreeMap::new(),
            activated_unix_secs: 1,
            cutback_pending: false,
            cutback: None,
            diagnostic: None,
        }
    }

    /// The derived entry the collected writer would have published for
    /// `activation`.
    fn entry_for(activation: &ActivationRecordV2) -> WorkspaceIndexEntry {
        WorkspaceIndexEntry {
            manifest: format!("workspace/{PROJECT}/manifest.json"),
            active_snapshot: Some(bbox_edge_sidecar::snapshot::active_snapshot_rel(
                PROJECT,
                &activation.snapshot_id,
            )),
            dirty_overlay: None,
            repo_materialization: None,
            code_source_selector: Some(activation.selector.clone()),
            code_source_generation: Some(activation.generation_id.clone()),
            git_overlay: None,
            git_overlay_managed: true,
        }
    }

    fn write_entry(edges_dir: &Path, entry: Option<WorkspaceIndexEntry>) {
        let mut index = ManifestIndex::load_or_new(edges_dir).unwrap();
        match entry {
            Some(entry) => index.upsert_workspace(PROJECT, entry),
            None => {
                index.workspaces.remove(PROJECT);
            }
        }
        index.write_atomic(edges_dir).unwrap();
    }

    fn loaded_entry(edges_dir: &Path) -> Option<WorkspaceIndexEntry> {
        ManifestIndex::load_or_new(edges_dir)
            .unwrap()
            .workspaces
            .get(PROJECT)
            .cloned()
    }

    fn edges_root(directory: &tempfile::TempDir) -> std::path::PathBuf {
        directory.path().canonicalize().unwrap().join("edges")
    }

    #[test]
    fn an_agreeing_entry_is_left_alone() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let current = activation(&"a".repeat(64));
        write_entry(&edges_dir, Some(entry_for(&current)));

        let state =
            classify_workspace_manifest(&edges_dir, &current, GenerationState::Active).unwrap();

        assert_eq!(state, WorkspaceManifestState::Agreed);
        assert_eq!(loaded_entry(&edges_dir), Some(entry_for(&current)));
    }

    /// The exact production shape: the journal names the generation whose
    /// state flip already landed, and the derived entry still names its
    /// predecessor because the crash swallowed the manifest write.
    #[test]
    fn a_stale_entry_behind_an_active_generation_is_reconciled() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let predecessor = activation(&"a".repeat(64));
        let current = activation(&"c".repeat(64));
        let mut stale = entry_for(&predecessor);
        stale.repo_materialization = Some("materialized/repo".into());
        write_entry(&edges_dir, Some(stale));

        let state =
            classify_workspace_manifest(&edges_dir, &current, GenerationState::Active).unwrap();

        assert_eq!(
            state,
            WorkspaceManifestState::Reconciled {
                previous_generation: Some(predecessor.generation_id.clone()),
            }
        );
        let entry = loaded_entry(&edges_dir).expect("reconciled entry");
        assert_eq!(
            entry.code_source_generation.as_deref(),
            Some(current.generation_id.as_str())
        );
        assert_eq!(
            entry.code_source_selector.as_deref(),
            Some(current.selector.as_str())
        );
        assert_eq!(
            entry.active_snapshot,
            Some(bbox_edge_sidecar::snapshot::active_snapshot_rel(
                PROJECT,
                &current.snapshot_id
            ))
        );
        // Carried over exactly as the production writer does.
        assert_eq!(
            entry.repo_materialization.as_deref(),
            Some("materialized/repo")
        );
        // An overlay is bound to one code generation and must not survive.
        assert!(entry.git_overlay.is_none());
        assert!(entry.git_overlay_managed);
    }

    /// The second production variant: the project never carried a collected
    /// entry, so the crash left no entry at all.
    #[test]
    fn an_absent_entry_behind_an_active_generation_is_reconciled() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let current = activation(&"c".repeat(64));
        write_entry(&edges_dir, None);

        let state =
            classify_workspace_manifest(&edges_dir, &current, GenerationState::Active).unwrap();

        assert_eq!(
            state,
            WorkspaceManifestState::Reconciled {
                previous_generation: None
            }
        );
        assert_eq!(loaded_entry(&edges_dir), Some(entry_for(&current)));
    }

    /// The state flip is written after the manifest write, so a non-Active
    /// generation whose entry names the predecessor is an activation that
    /// never finished. The manifest is right and must not be rewritten.
    #[test]
    fn a_stale_entry_behind_an_unsettled_generation_is_in_flight() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let predecessor = activation(&"a".repeat(64));
        let current = activation(&"c".repeat(64));
        write_entry(&edges_dir, Some(entry_for(&predecessor)));

        for state in [GenerationState::StagingIndex, GenerationState::Ready] {
            let classified = classify_workspace_manifest(&edges_dir, &current, state).unwrap();
            assert_eq!(
                classified,
                WorkspaceManifestState::ActivationInFlight {
                    journal_generation: current.generation_id.clone(),
                }
            );
            assert_eq!(loaded_entry(&edges_dir), Some(entry_for(&predecessor)));
        }
    }

    /// `cutback_to_local` writes the two stores in the inverse order, so its
    /// crash window is resolved in the manifest's favour elsewhere. Rewriting
    /// it here would re-point the project at the collected generation it was
    /// deliberately cut back from.
    #[test]
    fn the_cutback_crash_window_is_not_reconciled() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let current = activation(&"c".repeat(64));
        let local = WorkspaceIndexEntry {
            manifest: format!("workspace/{PROJECT}/manifest.json"),
            active_snapshot: Some(format!("workspace/{PROJECT}/snapshots/head-local")),
            dirty_overlay: None,
            repo_materialization: None,
            code_source_selector: Some(bbox_code_source::local_selector(PROJECT)),
            code_source_generation: Some("local".into()),
            git_overlay: None,
            git_overlay_managed: true,
        };
        write_entry(&edges_dir, Some(local.clone()));

        let state =
            classify_workspace_manifest(&edges_dir, &current, GenerationState::Active).unwrap();

        assert_eq!(state, WorkspaceManifestState::Disagrees);
        assert_eq!(loaded_entry(&edges_dir), Some(local));
    }

    /// Only a well-formed predecessor entry is a recognized torn write.
    /// Anything else is drift, and drift still refuses.
    #[test]
    fn a_malformed_predecessor_entry_still_disagrees() {
        let current = activation(&"c".repeat(64));
        let predecessor = activation(&"a".repeat(64));

        let cross_project = {
            let mut entry = entry_for(&predecessor);
            entry.manifest = "workspace/p_00000000000000000000000000000009/manifest.json".into();
            entry
        };
        let traversal = {
            let mut entry = entry_for(&predecessor);
            entry.active_snapshot = Some(format!("workspace/{PROJECT}/snapshots/../../escape"));
            entry
        };
        let no_snapshot = {
            let mut entry = entry_for(&predecessor);
            entry.active_snapshot = None;
            entry
        };

        for malformed in [cross_project, traversal, no_snapshot] {
            let directory = tempfile::tempdir().unwrap();
            let edges_dir = edges_root(&directory);
            write_entry(&edges_dir, Some(malformed.clone()));

            let state =
                classify_workspace_manifest(&edges_dir, &current, GenerationState::Active).unwrap();

            assert_eq!(state, WorkspaceManifestState::Disagrees);
            assert_eq!(loaded_entry(&edges_dir), Some(malformed));
        }
    }

    /// A local activation record is not this module's to repair.
    #[test]
    fn a_local_activation_is_out_of_scope() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = edges_root(&directory);
        let mut local = activation(&"c".repeat(64));
        local.selector = bbox_code_source::local_selector(PROJECT);
        write_entry(&edges_dir, None);

        let state =
            classify_workspace_manifest(&edges_dir, &local, GenerationState::Active).unwrap();

        assert_eq!(state, WorkspaceManifestState::Disagrees);
        assert_eq!(loaded_entry(&edges_dir), None);
    }
}

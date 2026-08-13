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

use std::path::Path;

use anyhow::{Context, Result};
use bbox_code_source::GenerationState;
use bbox_code_source_store::ActivationRecordV2;
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
            // torn shape. An entry that is malformed, cross-project, or
            // traversal bearing is drift, and drift still refuses.
            if !predecessor_collected_entry_is_well_formed(entry, project_id) {
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

/// A predecessor entry has to look like something the collected writer
/// produced for THIS project before it can be treated as a torn write rather
/// than drift. Mirrors the shape validation the relationship chain applies to
/// its own crash-window admission.
fn predecessor_collected_entry_is_well_formed(
    entry: &WorkspaceIndexEntry,
    project_id: &str,
) -> bool {
    let Some(selector) = entry.code_source_selector.as_deref() else {
        return false;
    };
    if !selector.starts_with("collected:") || entry.code_source_generation.is_none() {
        return false;
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

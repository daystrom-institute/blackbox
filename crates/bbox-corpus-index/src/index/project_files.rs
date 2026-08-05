use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use ignore::{DirEntry, WalkBuilder};
use sha2::{Digest, Sha256};
use tantivy::collector::{Count, TopDocs};
use tantivy::query::TermQuery;
use tantivy::schema::IndexRecordOption;
use tantivy::{Index, IndexWriter, TantivyDocument, Term};

use super::{FieldHandles, FileMeta, FileMetaSource, ReindexConfig};
use bbox_chunker::{self as chunker, Chunk, Edge, EdgeConfidence, EdgeProvenance};
use bbox_corpus_core::code_project_identity::CodeProjectIdentity;
use bbox_corpus_core::entity_ref::{self, EntityRef};
use bbox_corpus_core::project_record::ProjectRecord;

/// Version-1 project-file document fields that [`CodeProjectIdentity`]
/// deliberately does not carry, resolved by the caller from the attached
/// `ProjectRecord` when one exists (Phase 3 plan section 6 items 1 and 5).
///
/// Both fields are host-local compatibility values that the P3-E schema cut
/// removes; until then a document must keep emitting exactly what it emits
/// today for a bridge project, and a remote-only catalog project simply has
/// neither value.
///
/// `repo_id` is the version-1 record's repo id (a hash of the repository's
/// first commit), NOT the published scope's recorded `repo_id` (an operator
/// authority string from the repo's committed config). The two are different
/// values; this document field has always meant the first one, so it is
/// threaded rather than re-derived from the scope.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProjectFileCompatFields<'a> {
    pub repo_id: Option<&'a str>,
}

/// Caller-supplied checkout roots for one project indexing operation.
///
/// The daemon indexing layer obtains these roots from validated leases and
/// retains the leases for the entire lower-layer call. This crate deliberately
/// knows nothing about the authority or lease types.
#[derive(Clone, Copy)]
pub struct ProjectIndexAccess<'a> {
    /// Source-neutral identity of the project being indexed. Present for
    /// every planned project, including one with zero attachments, so the
    /// pass never has to project an identity out of a path-bearing record.
    pub identity: &'a bbox_corpus_core::code_project_identity::CodeProjectIdentity,
    /// The version-1 compatibility record, present exactly when the project
    /// has an attached checkout this pass. `None` is the detached and
    /// remote-only case: every lane that needs a checkout path (local walk,
    /// Git history) is `None` alongside it by construction.
    pub project: Option<&'a ProjectRecord>,
    pub local_root: Option<&'a Path>,
    pub git_root: Option<&'a Path>,
}

impl ProjectIndexAccess<'_> {
    pub fn project_id(&self) -> &str {
        self.identity.project_id.as_str()
    }
}

/// Cheap "does this root contain at least one indexable file?" probe for the
/// H3 empty-scan refusal (Phase 3 plan section 7 item 2). Deliberately NOT a
/// full scan: it stops at the first admissible entry, so the common
/// non-empty case costs a handful of stats instead of a second walk of the
/// whole checkout. It applies the same admission rules as
/// [`scan_project_files`], because "indexable" must mean the same thing in
/// both places or the refusal fires on roots the pass would legitimately
/// scan as empty.
pub fn project_root_has_indexable_entry(root: &Path, _config: &ReindexConfig) -> bool {
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_entry(entry))
        .build();
    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let Ok(meta) = fs::symlink_metadata(path) else {
            continue;
        };
        if meta.file_type().is_symlink() || !meta.is_file() {
            continue;
        }
        let Some(max_bytes) = bbox_code_source::max_bytes_for_path(path) else {
            continue;
        };
        if meta.len() > max_bytes || path.to_str().is_none() {
            continue;
        }
        return true;
    }
    false
}

#[derive(Debug, Default)]
pub struct ProjectIndexStats {
    pub indexed_files: u64,
    pub indexed_docs: u64,
    pub skipped: u64,
    pub emitted_edges: u64,
    pub indexed_commits: u64,
    pub call_edges: u64,
    pub resolved_call_edges: u64,
    pub skipped_symlinks: u64,
    pub skipped_special: u64,
    pub skipped_unsupported: u64,
    pub skipped_oversize: u64,
    pub pending_local_snapshots: Vec<bbox_edge_sidecar::snapshot::PendingLocalSnapshotActivation>,
    pub publication: ProjectIndexPublicationBundle,
    /// `project_id -> new selector` for every collected generation this pass
    /// migrated off an outgoing materialization version. The caller republishes
    /// the pinned selector map from it, since the in-memory map was seeded from
    /// the pre-flip manifest.
    pub migrated_collected_selectors: BTreeMap<String, String>,
}

#[derive(Debug, Default)]
pub struct ProjectIndexPublicationBundle {
    actions: Vec<ProjectIndexPublication>,
}

#[derive(Debug)]
enum ProjectIndexPublication {
    SnapshotRename {
        staged: PathBuf,
        destination: PathBuf,
    },
    ProjectEdges {
        edges_dir: PathBuf,
        project_id: String,
        edges: Vec<Edge>,
        deleted_rel_hashes: std::collections::HashSet<String>,
        compact_legacy: bool,
    },
    GitHistory(super::git_history::GitHistoryPublication),
    SnapshotGitCurrent {
        edges_dir: PathBuf,
        project_id: String,
        snapshot_id: String,
        include_managed_git: bool,
    },
    LocalSnapshot(LocalSnapshotPublication),
}

#[derive(Debug)]
struct LocalSnapshotPublication {
    edges_dir: PathBuf,
    project_id: String,
    repo_id: String,
    branch: Option<String>,
    head_sha: String,
    dirty: bool,
    dirty_fingerprint: Option<String>,
    snapshot_id: String,
}

impl ProjectIndexPublicationBundle {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    pub(crate) fn stage_git_history(
        &mut self,
        publication: super::git_history::GitHistoryPublication,
    ) {
        self.actions
            .push(ProjectIndexPublication::GitHistory(publication));
    }

    pub fn stage_snapshot_git_current(
        &mut self,
        edges_dir: &Path,
        project_id: &str,
        snapshot_id: &str,
        include_managed_git: bool,
    ) {
        self.actions
            .push(ProjectIndexPublication::SnapshotGitCurrent {
                edges_dir: edges_dir.to_path_buf(),
                project_id: project_id.to_string(),
                snapshot_id: snapshot_id.to_string(),
                include_managed_git,
            });
    }

    /// Publish every filesystem-derived effect staged by the corpus pass.
    /// The caller must hold the checkout publication guard for all leases that
    /// contributed to this bundle for the entire call and the Tantivy commit.
    pub fn publish(&mut self) -> Result<PublicationResult> {
        let mut result = PublicationResult {
            pending_local_snapshots: Vec::new(),
            pending_snapshot_finalizations: Vec::new(),
            commit_succeeded: false,
        };
        for action in self.actions.drain(..) {
            match action {
                ProjectIndexPublication::SnapshotRename {
                    staged,
                    destination,
                } => {
                    fs::rename(&staged, &destination)?;
                    if let Some(parent) = destination.parent() {
                        fs::File::open(parent)?.sync_all()?;
                    }
                }
                ProjectIndexPublication::ProjectEdges {
                    edges_dir,
                    project_id,
                    edges,
                    deleted_rel_hashes,
                    compact_legacy,
                } => {
                    bbox_edge_sidecar::edge_sidecar::replace_materialized_edges_incremental(
                        &edges_dir,
                        "project",
                        &project_id,
                        &edges,
                    )?;
                    bbox_edge_sidecar::edge_sidecar::purge_managed_edges_for_path_hashes(
                        &edges_dir,
                        "project",
                        &project_id,
                        &deleted_rel_hashes,
                    )?;
                    if compact_legacy
                        && let Err(error) = bbox_edge_sidecar::edge_sidecar::compact_legacy_sidecar(
                            &edges_dir,
                            &project_id,
                            true,
                        )
                    {
                        tracing::warn!(
                            project_id = %project_id,
                            error = %error,
                            "failed to compact legacy edge sidecar after full project refresh"
                        );
                    }
                }
                ProjectIndexPublication::GitHistory(publication) => publication.publish()?,
                ProjectIndexPublication::SnapshotGitCurrent {
                    edges_dir,
                    project_id,
                    snapshot_id,
                    include_managed_git,
                } => {
                    let git_edges = if include_managed_git {
                        bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
                            &edges_dir,
                            "git",
                            &project_id,
                        )?
                        .into_iter()
                        .map(|edge| bbox_edge_sidecar::edge_sidecar::Edge {
                            source: edge.source,
                            kind: edge.kind,
                            target: edge.target,
                            provenance: edge.provenance,
                            confidence: edge.confidence,
                            metadata: Default::default(),
                        })
                        .collect::<Vec<_>>()
                    } else {
                        Vec::new()
                    };
                    let txn_handle =
                        bbox_edge_sidecar::snapshot::write_snapshot_members_transaction(
                            &edges_dir,
                            &project_id,
                            &snapshot_id,
                            &[("git-current.jsonl", git_edges.as_slice())],
                        )?;
                    result.pending_snapshot_finalizations.push(txn_handle);
                }
                ProjectIndexPublication::LocalSnapshot(publication) => {
                    let project_edges =
                        bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
                            &publication.edges_dir,
                            "project",
                            &publication.project_id,
                        )?;
                    let git_edges = bbox_edge_sidecar::edge_sidecar::read_managed_derived_edges(
                        &publication.edges_dir,
                        "git",
                        &publication.project_id,
                    )?;
                    result.pending_local_snapshots.push(
                        bbox_edge_sidecar::snapshot::stage_local_snapshot_activation(
                            &publication.edges_dir,
                            &publication.project_id,
                            &publication.repo_id,
                            publication.branch.as_deref(),
                            &publication.head_sha,
                            publication.dirty,
                            publication.dirty_fingerprint.as_deref(),
                            &publication.snapshot_id,
                            &project_edges,
                            &[],
                            &git_edges,
                        )?,
                    );
                }
            }
        }
        Ok(result)
    }
}

impl Drop for ProjectIndexPublicationBundle {
    fn drop(&mut self) {
        for action in &self.actions {
            if let ProjectIndexPublication::SnapshotRename { staged, .. } = action {
                let _ = fs::remove_file(staged);
            }
        }
    }
}

#[derive(Debug)]
pub struct PublicationResult {
    pub pending_local_snapshots: Vec<bbox_edge_sidecar::snapshot::PendingLocalSnapshotActivation>,
    /// Transaction handles for snapshots that had their member files staged
    /// during publish(). The caller MUST carry each handle's txn_token in
    /// the Tantivy commit payload (prepare_commit + set_payload) and call
    /// finalize_snapshot_publication for each handle AFTER writer.commit()
    /// succeeds. R20F2: finalization processes ONLY the exact handle.
    pub pending_snapshot_finalizations: Vec<bbox_edge_sidecar::snapshot::SnapshotTxnHandle>,
    commit_succeeded: bool,
}

impl PublicationResult {
    pub fn rollback_pending(&mut self) -> Result<()> {
        let mut failures = Vec::new();
        self.pending_snapshot_finalizations.retain(|handle| {
            match bbox_edge_sidecar::snapshot::discard_snapshot_transaction(handle) {
                Ok(()) => false,
                Err(error) => {
                    failures.push(format!(
                        "{}:{}: {error:#}",
                        handle.project_id, handle.txn_token
                    ));
                    true
                }
            }
        });
        if failures.is_empty() {
            Ok(())
        } else {
            anyhow::bail!(
                "snapshot transaction rollback left unresolved staging: {}",
                failures.join("; ")
            )
        }
    }

    pub fn mark_commit_succeeded(&mut self) {
        self.commit_succeeded = true;
    }

    pub fn take_pending_snapshot_finalizations(
        &mut self,
    ) -> Vec<bbox_edge_sidecar::snapshot::SnapshotTxnHandle> {
        std::mem::take(&mut self.pending_snapshot_finalizations)
    }

    pub fn take_pending_local_snapshots(
        &mut self,
    ) -> Vec<bbox_edge_sidecar::snapshot::PendingLocalSnapshotActivation> {
        std::mem::take(&mut self.pending_local_snapshots)
    }

    /// Finalize all pending transactions. R20F4: returns Result; the caller
    /// must NOT publish the post-commit read view until this succeeds.
    pub fn finalize_publications(mut self) -> Result<()> {
        self.commit_succeeded = true;
        for handle in &self.pending_snapshot_finalizations {
            if let Err(error) = bbox_edge_sidecar::snapshot::finalize_snapshot_publication(handle) {
                tracing::error!(
                    project_id = %handle.project_id,
                    snapshot_id = %handle.snapshot_id,
                    txn_token = %handle.txn_token,
                    error = %error,
                    "failed to finalize snapshot publication after index commit"
                );
                return Err(error);
            }
        }
        Ok(())
    }
}

impl Drop for PublicationResult {
    fn drop(&mut self) {
        if self.commit_succeeded || self.pending_snapshot_finalizations.is_empty() {
            return;
        }
        if let Err(error) = self.rollback_pending() {
            tracing::error!(
                error = %error,
                "snapshot publication dropped with unresolved staged transactions"
            );
        }
    }
}

impl PublicationResult {
    /// Collect all txn_tokens from pending finalizations for the Tantivy
    /// commit payload.
    pub fn pending_txn_tokens(&self) -> Vec<String> {
        self.pending_snapshot_finalizations
            .iter()
            .map(|h| h.txn_token.clone())
            .collect()
    }

    /// R21F2: collect all cryptographic commitments from pending finalizations
    /// for the Tantivy commit payload. Each commitment is
    /// {project_id}:{txn_token}:{sha256(canonical_journal_bytes)}.
    pub fn pending_commitments(&self) -> Vec<String> {
        self.pending_snapshot_finalizations
            .iter()
            .map(|h| h.commitment().to_string())
            .collect()
    }
}

#[derive(Debug)]
pub struct CollectedIndexResult {
    pub snapshot_id: String,
    pub selector: String,
    pub document_count: u64,
    pub entity_inventory_sha256: String,
    pub current_chunk_targets: HashMap<String, EntityRef>,
    pub head_commit: String,
    pub dirty_fingerprint: String,
    pub worktree_dirty: bool,
}

pub fn collected_materialization_selector(project_id: &str, generation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-collected-selector-materialization-v1");
    hasher.update(bbox_edge_sidecar::snapshot::current_materialization_version().as_bytes());
    format!(
        "{}:m{}",
        bbox_code_source::source_selector(project_id, generation_id),
        hex::encode(&hasher.finalize()[..8])
    )
}

pub fn local_activation_marker(project_id: &str) -> String {
    format!("code-source-activation:{project_id}")
}

pub fn recover_pending_local_snapshot_activations(
    searcher: &tantivy::Searcher,
    fields: FieldHandles,
    edges_dir: &Path,
) -> Result<()> {
    let pins = bbox_edge_sidecar::snapshot::load_pending_local_activation_pins(edges_dir)?;
    if pins.is_empty() {
        return Ok(());
    }
    let mut committed = 0_usize;
    for pin in &pins {
        let query = TermQuery::new(
            Term::from_field_text(fields.entity_id, &local_activation_marker(pin.project_id())),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count > 1 {
            anyhow::bail!("local activation marker is not unique");
        }
        let matches_commit = searcher
            .search(&query, &TopDocs::with_limit(1))?
            .into_iter()
            .next()
            .map(|(_score, address)| searcher.doc::<TantivyDocument>(address))
            .transpose()?
            .and_then(|document| {
                document
                    .get_first(fields.code_source_generation)
                    .and_then(|value| match value {
                        tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                        _ => None,
                    })
            })
            .is_some_and(|token| token == pin.commit_token());
        if matches_commit {
            committed += 1;
        }
    }

    if committed == pins.len() {
        let activations = pins
            .iter()
            .map(|pin| pin.activation().clone())
            .collect::<Vec<_>>();
        bbox_edge_sidecar::snapshot::activate_pending_local_snapshots(edges_dir, &activations)?;
    } else if committed != 0 {
        anyhow::bail!("local activation commit markers are only partially visible");
    }
    bbox_edge_sidecar::snapshot::clear_pending_local_activation_pins(edges_dir)
}

#[derive(Debug, Default)]
struct ProjectFileScanStats {
    skipped_symlinks: u64,
    skipped_special: u64,
    skipped_unsupported: u64,
    skipped_oversize: u64,
}

#[derive(Debug, Clone)]
pub struct ActiveCollectedSource {
    pub selector: String,
    pub generation_id: String,
}

/// Where a project's PERSISTED collected materialization sits relative to the
/// version the running binary mints.
///
/// Why this classification exists: `collected_materialization_selector` and
/// `collected_snapshot_id` both fold `current_materialization_version()`, which
/// folds `INDEXER_VERSION`. A version bump therefore changes the selector's
/// `m` suffix and the snapshot id BY CONSTRUCTION, for every already-active
/// collected generation, with nothing wrong on disk. Treating that as a refusal
/// (which it was before P3-E) wedges the very full rebuild the paired
/// `INDEX_SCHEMA_VERSION` bump forces at the first open after deploy, and
/// therefore wedges boot.
///
/// The discriminator is whether the outgoing state is INTERNALLY CONSISTENT: a
/// well-formed collected materialization selector for the SAME
/// `(project_id, generation_id)`, a well-formed collected snapshot id, and an
/// activation record that agrees with the persisted selector. Anything else -
/// a different generation, a different project, an activation record that
/// disagrees with the manifest, a malformed selector or snapshot id - is
/// genuinely inconsistent and still fails closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollectedMaterializationState {
    /// The persisted selector and snapshot id are exactly what this binary
    /// mints. Nothing to migrate.
    Current,
    /// Minted under an outgoing materialization version. Re-stage under the
    /// current one and move the durable pointers with it.
    Outgoing,
}

/// A collected snapshot id is `collected-` plus 32 lowercase hex (16 bytes),
/// per `bbox_edge_sidecar::snapshot::collected_snapshot_id`. Shape-only: the
/// outgoing version's digest cannot be re-derived, which is precisely why the
/// migration arm exists.
fn is_collected_snapshot_id_shape(value: &str) -> bool {
    match value.strip_prefix("collected-") {
        Some(hex) => {
            hex.len() == 32
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || byte.is_ascii_lowercase() && byte <= b'f')
        }
        None => false,
    }
}

pub fn classify_collected_materialization(
    project_id: &str,
    active: &ActiveCollectedSource,
    activation: &bbox_code_source_store::ActivationRecord,
) -> Result<CollectedMaterializationState> {
    let expected_selector = collected_materialization_selector(project_id, &active.generation_id);
    let expected_snapshot =
        bbox_edge_sidecar::snapshot::collected_snapshot_id(project_id, &active.generation_id);
    if active.selector == expected_selector && activation.snapshot_id == expected_snapshot {
        return Ok(CollectedMaterializationState::Current);
    }
    // A selector at the current suffix whose snapshot id is NOT current (or the
    // reverse) cannot be an outgoing version: both derive from the same version
    // string, so they move together or the state is corrupt.
    if active.selector == expected_selector || activation.snapshot_id == expected_snapshot {
        anyhow::bail!("active collected materialization version requires an explicit migration");
    }
    // Shape-only per `validate_collected_materialization_selector`: it accepts
    // any historic 16-hex suffix, and pins the project and generation.
    if bbox_code_source::validate_collected_materialization_selector(
        project_id,
        &active.generation_id,
        &active.selector,
    )
    .is_err()
        || !is_collected_snapshot_id_shape(&activation.snapshot_id)
    {
        anyhow::bail!("active collected selector requires materialization migration");
    }
    Ok(CollectedMaterializationState::Outgoing)
}

/// What the stale-path purge should do with one stale freshness row.
///
/// Shared by BOTH purge loops (the reindex pass and the legacy
/// `build_index` path) so the F2 exemptions can never drift apart: the two
/// loops were identical by copy before Phase 3 and the plan requires them to
/// move in lockstep.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StalePurgeAction {
    /// Exempt, and the freshness row stays: it is the preservation
    /// authority for this project's last-good local documents.
    ExemptRetainRow,
    /// Exempt, and the freshness row is dropped: an active collected
    /// generation serves the project now, so its local freshness rows carry
    /// no preservation obligation. This is the pre-Phase-3 behavior for the
    /// collected arm, preserved exactly.
    ExemptDropRow,
    /// Delete the project-file documents keyed by this source entry key.
    DeleteProjectEntry(String),
    /// Delete by the absolute `file_path` term. Transcripts, session
    /// artifacts, and pre-`LocalProjectFile` legacy rows key this way; the
    /// lane is untouched by Phase 3.
    DeleteByPath,
}

/// Classify one stale freshness row against the pass's purge exemptions
/// (Phase 3 plan section 7 item 2). `exempt_project_ids` is every project
/// whose plan is not `Local`-scanned this pass: collected, unavailable,
/// cutback-pending, warming-without-a-local-source, detached, and
/// empty-root-refused. `collected_project_ids` is the subset an active
/// collected generation serves, which is the only exempt arm whose
/// freshness rows are dropped rather than retained.
pub fn classify_stale_meta_row(
    source: Option<&FileMetaSource>,
    exempt_project_ids: &BTreeSet<String>,
    collected_project_ids: &BTreeSet<String>,
) -> StalePurgeAction {
    match source {
        Some(FileMetaSource::LocalProjectFile {
            project_id,
            entry_key,
            ..
        }) => {
            if !exempt_project_ids.contains(project_id) {
                StalePurgeAction::DeleteProjectEntry(entry_key.clone())
            } else if collected_project_ids.contains(project_id) {
                StalePurgeAction::ExemptDropRow
            } else {
                StalePurgeAction::ExemptRetainRow
            }
        }
        _ => StalePurgeAction::DeleteByPath,
    }
}

#[derive(Debug, Default)]
pub struct PreservedCollectedDocuments {
    pub project_ids: BTreeSet<String>,
    pub documents: Vec<TantivyDocument>,
}

/// Verified preservation of a project's LOCAL documents across a full
/// rebuild for a project this pass does not scan (Phase 3 plan section 7
/// item 2, closing F2/H1). The verification authority is the project's own
/// freshness rows: the per-project `FileMeta` set enumerates the files whose
/// documents must still be live, and the live document set must carry
/// exactly that entry-key inventory.
///
/// A mismatch records `preservation_failed` and returns an error BEFORE the
/// caller reaches `delete_all_documents()`, exactly like the collected arm
/// ([`collect_preserved_collected_documents`]). Convergence is through the
/// operator surfaces only: an acknowledged purge, detach/unregister, or
/// retire. There is deliberately no unverified-preservation downgrade.
pub fn collect_verified_detached_documents(
    index: &Index,
    config: &ReindexConfig,
    f: FieldHandles,
    project_ids: &BTreeSet<String>,
    meta: &HashMap<String, FileMeta>,
) -> Result<Vec<TantivyDocument>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut expected: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for row in meta.values() {
        if let FileMetaSource::LocalProjectFile {
            project_id,
            entry_key,
            ..
        } = &row.source
            && project_ids.contains(project_id.as_str())
        {
            expected
                .entry(project_id.as_str())
                .or_default()
                .insert(entry_key.as_str());
        }
    }
    let searcher = index.reader()?.searcher();
    let mut store = None;
    let mut documents = Vec::new();
    for project_id in project_ids {
        let selector = bbox_code_source::local_selector(project_id);
        let query = TermQuery::new(
            Term::from_field_text(f.code_source_selector, &selector),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        let expected_keys = expected.remove(project_id.as_str()).unwrap_or_default();
        if count == 0 && expected_keys.is_empty() {
            // Nothing indexed and nothing promised: not a preservation case.
            continue;
        }
        let mut observed_keys: BTreeSet<String> = BTreeSet::new();
        let mut project_documents = Vec::with_capacity(count);
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            let document = searcher.doc::<TantivyDocument>(address)?;
            if let Some(entry_key) = document
                .get_first(f.code_source_entry_key)
                .and_then(|value| match value {
                    tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                    _ => None,
                })
            {
                observed_keys.insert(entry_key);
            }
            project_documents.push(document);
        }
        let expected_owned: BTreeSet<String> =
            expected_keys.iter().map(|key| (*key).to_string()).collect();
        if observed_keys != expected_owned {
            let diagnostic = format!(
                "detached project preservation inventory mismatch: freshness rows list {} \
                 file(s), live documents carry {} distinct entry key(s)",
                expected_owned.len(),
                observed_keys.len()
            );
            let store = match store.as_ref() {
                Some(store) => store,
                None => {
                    store = Some(bbox_code_source_store::CodeSourceStore::open(
                        &config.code_source_store_path,
                        bbox_code_source_store::StoreLimits::default(),
                    )?);
                    store.as_ref().expect("store was just installed")
                }
            };
            store.record_health_failure(project_id, "preservation_failed", &diagnostic)?;
            anyhow::bail!("{diagnostic} (project {project_id})");
        }
        documents.extend(project_documents);
    }
    Ok(documents)
}

pub fn collect_project_documents(
    index: &Index,
    f: FieldHandles,
    project_ids: &BTreeSet<String>,
) -> Result<Vec<TantivyDocument>> {
    if project_ids.is_empty() {
        return Ok(Vec::new());
    }
    let searcher = index.reader()?.searcher();
    let mut documents = Vec::new();
    for project_id in project_ids {
        let selector = bbox_code_source::local_selector(project_id);
        let query = TermQuery::new(
            Term::from_field_text(f.code_source_selector, &selector),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count == 0 {
            continue;
        }
        for (_score, address) in searcher.search(&query, &TopDocs::with_limit(count))? {
            documents.push(searcher.doc::<TantivyDocument>(address)?);
        }
    }
    Ok(documents)
}

pub fn active_collected_sources(
    config: &ReindexConfig,
) -> Result<BTreeMap<String, ActiveCollectedSource>> {
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let manifest = bbox_edge_sidecar::manifest::ManifestIndex::load_or_new(&edges_dir)?;
    Ok(manifest
        .workspaces
        .into_iter()
        .filter_map(|(project_id, entry)| {
            let selector = entry.code_source_selector?;
            let generation_id = entry.code_source_generation?;
            selector.starts_with("collected:").then_some((
                project_id,
                ActiveCollectedSource {
                    selector,
                    generation_id,
                },
            ))
        })
        .collect())
}

pub fn collect_preserved_collected_documents(
    index: &Index,
    config: &ReindexConfig,
    f: FieldHandles,
) -> Result<PreservedCollectedDocuments> {
    let active = active_collected_sources(config)?;
    if active.is_empty() {
        return Ok(PreservedCollectedDocuments::default());
    }
    let store = bbox_code_source_store::CodeSourceStore::open(
        &config.code_source_store_path,
        bbox_code_source_store::StoreLimits::default(),
    )?;
    let searcher = index.reader()?.searcher();
    let mut preserved = PreservedCollectedDocuments::default();
    for (project_id, source) in active {
        let Some(activation) = store.load_activation(&project_id)? else {
            let diagnostic = "active collected source has no activation record";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        };
        if activation.selector != source.selector
            || activation.generation_id != source.generation_id
        {
            let diagnostic = "active collected source disagrees with its activation record";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        }
        let generation = store.find_generation(&source.generation_id)?;
        if generation.materialized_doc_count != Some(activation.document_count)
            || generation.entity_inventory_sha256.as_deref()
                != Some(activation.entity_inventory_sha256.as_str())
        {
            let diagnostic = "active collected materialization metadata is incomplete";
            store.record_health_failure(&project_id, "preservation_failed", diagnostic)?;
            anyhow::bail!(diagnostic);
        }
        let query = TermQuery::new(
            Term::from_field_text(f.code_source_selector, &source.selector),
            IndexRecordOption::Basic,
        );
        let count = searcher.search(&query, &Count)?;
        if count as u64 != activation.document_count {
            store.record_health_failure(
                &project_id,
                "preservation_failed",
                &format!(
                    "active collected document count mismatch: expected {}, observed {}",
                    activation.document_count, count
                ),
            )?;
            anyhow::bail!(
                "active collected document count mismatch: expected {}, observed {}",
                activation.document_count,
                count
            );
        }
        let mut entity_ids = Vec::with_capacity(count);
        let mut documents = Vec::with_capacity(count);
        let hits = if count == 0 {
            Vec::new()
        } else {
            searcher.search(&query, &TopDocs::with_limit(count))?
        };
        for (_score, address) in hits {
            let document = searcher.doc::<TantivyDocument>(address)?;
            let entity_id = document
                .get_first(f.entity_id)
                .and_then(|value| match value {
                    tantivy::schema::OwnedValue::Str(value) => Some(value.clone()),
                    _ => None,
                })
                .ok_or_else(|| anyhow::anyhow!("preserved collected document has no entity id"))?;
            entity_ids.push(entity_id);
            documents.push(document);
        }
        entity_ids.sort();
        let mut inventory = Sha256::new();
        for entity_id in entity_ids {
            inventory.update((entity_id.len() as u64).to_be_bytes());
            inventory.update(entity_id.as_bytes());
        }
        let observed = hex::encode(inventory.finalize());
        if observed != activation.entity_inventory_sha256 {
            store.record_health_failure(
                &project_id,
                "preservation_failed",
                &format!(
                    "active collected entity inventory mismatch: expected {}, observed {}",
                    activation.entity_inventory_sha256, observed
                ),
            )?;
            anyhow::bail!(
                "active collected entity inventory mismatch: expected {}, observed {}",
                activation.entity_inventory_sha256,
                observed
            );
        }
        store.clear_health_failure(&project_id, "preservation_failed")?;
        preserved.project_ids.insert(project_id);
        preserved.documents.extend(documents);
    }
    Ok(preserved)
}

struct PendingProjectFile {
    /// The P3-E composite freshness key (`pf\0<pid>\0<kind>\0<relpath>`), not
    /// a host path: the meta map is keyed by it, so carrying the absolute path
    /// here would only invite a caller to re-key by it.
    meta_key: String,
    relative_path: String,
    mtime: u64,
    size: u64,
    chunks: Vec<Chunk>,
}

struct ProjectIndexContext<'a> {
    f: FieldHandles,
    writer: &'a mut IndexWriter,
    meta: &'a mut HashMap<String, FileMeta>,
    stats: &'a mut ProjectIndexStats,
    edges_dir: &'a Path,
    git_meta_dir: &'a Path,
    force_git_full: bool,
    /// The identity's display name, the only value the `project` field of a
    /// project-file or commit document may carry after the P3-E cut.
    project_display: &'a str,
}

fn project_refs_v2_enabled() -> bool {
    std::env::var("BBOX_PROJECT_REFS_V2")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

fn ref_snapshot_id(
    project: &ProjectRecord,
    root: &Path,
    files: &[(String, u64, u64)],
    commit_sha: Option<&str>,
) -> Option<String> {
    if !project_refs_v2_enabled() {
        return None;
    }
    if let (Some(repo_id), Some(head_sha)) = (project.repo_id.as_deref(), commit_sha) {
        return Some(bbox_edge_sidecar::snapshot::clean_snapshot_id(
            repo_id,
            &project.project_id,
            head_sha,
        ));
    }

    let mut hasher = Sha256::new();
    hasher.update(root.to_string_lossy().as_bytes());
    for (path, mtime, size) in files {
        hasher.update(path.as_bytes());
        hasher.update(mtime.to_le_bytes());
        hasher.update(size.to_le_bytes());
    }
    let fingerprint = hex::encode(hasher.finalize());
    Some(bbox_edge_sidecar::snapshot::nongit_snapshot_id(
        &project.project_id,
        &fingerprint,
    ))
}

/// The pass's current freshness-key set, in the SAME key space the meta map
/// uses: project rows carry the P3-E composite key, the `git:<project_id>`
/// history source key stays as it was, and no absolute path appears for a
/// project row. The purge loops diff meta keys against this set, so a mixed
/// key space here would purge every project row on the first pass after the
/// cut.
pub fn scan_project_files_with_access(
    config: &ReindexConfig,
    projects: &[ProjectIndexAccess<'_>],
) -> Result<Vec<(String, u64, u64)>> {
    let mut files = Vec::new();
    let collected = active_collected_sources(config)?;
    for access in projects {
        let project_id = access.project_id();
        if !collected.contains_key(project_id)
            && let Some(root) = access.local_root
        {
            let mut scanned = Vec::new();
            let _ = scan_project_files(&root, &mut scanned)?;
            let selector = bbox_code_source::local_selector(project_id);
            let source_kind = bbox_code_source::source_kind_for_selector(&selector);
            files.extend(scanned.into_iter().map(|(path_str, mtime, size)| {
                (
                    bbox_code_source::project_file_meta_key(
                        project_id,
                        source_kind,
                        &local_relative_path(root, Path::new(&path_str)),
                    ),
                    mtime,
                    size,
                )
            }));
        }
        if access
            .project
            .is_some_and(|project| project.repo_id.is_some())
            && let Some(git_root) = access.git_root
            && let Some(head) = bbox_corpus_core::git::head_fingerprint(git_root)
        {
            files.push((super::git_history::git_source_key(project_id), 0, head));
        }
    }
    Ok(files)
}

pub fn index_projects_with_access(
    config: &ReindexConfig,
    projects: &[ProjectIndexAccess<'_>],
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    force_git_full: bool,
    preserved_collected: &BTreeSet<String>,
) -> Result<ProjectIndexStats> {
    let mut stats = ProjectIndexStats::default();
    let edges_dir =
        bbox_edge_sidecar::edge_sidecar::edges_dir_from_projects_path(&config.projects_path);
    let git_meta_dir = super::git_history::git_meta_dir_from_projects_path(&config.projects_path);
    let collected = active_collected_sources(config)?;
    let collected_store = (!collected.is_empty()).then(|| {
        bbox_code_source_store::CodeSourceStore::open(
            &config.code_source_store_path,
            bbox_code_source_store::StoreLimits::default(),
        )
    });
    for access in projects {
        let project_id = access.project_id();
        if let Some(active) = collected.get(project_id) {
            let store = collected_store
                .as_ref()
                .expect("collected store exists when collected sources exist")
                .as_ref()
                .map_err(|error| anyhow::anyhow!(error.to_string()))?;
            index_active_collected_project(
                access.identity,
                access.project,
                access.git_root,
                active,
                store,
                f,
                writer,
                meta,
                &edges_dir,
                &git_meta_dir,
                force_git_full,
                preserved_collected.contains(project_id),
                &mut stats,
            )?;
            continue;
        }
        // Only an attached project can be walked locally; a detached or
        // remote-only project reaches here with both `project` and
        // `local_root` absent and is a pass-level no-op by construction.
        let Some(project) = access.project else {
            continue;
        };
        let Some(root) = access.local_root else {
            // Source planning already recorded the durable `source_unavailable`
            // health for this project (Phase 3 plan section 7 item 1); this
            // stays as a pass-local breadcrumb, not the only trace.
            tracing::debug!(
                project_id = %project.project_id,
                "local project unavailable; retaining its last-good indexed generation"
            );
            continue;
        };
        let mut ctx = ProjectIndexContext {
            f,
            writer,
            meta,
            stats: &mut stats,
            edges_dir: &edges_dir,
            git_meta_dir: &git_meta_dir,
            force_git_full,
            project_display: access.identity.display_name.as_str(),
        };
        index_project(project, root, access.git_root, &mut ctx)?;
    }
    Ok(stats)
}

/// Move a project's durable collected pointers from an OUTGOING
/// materialization onto the one the caller just re-staged.
///
/// Ordering mirrors `activate_desired_loop`'s activation transaction, which is
/// the only other writer of these records: record the new materialization
/// inventory, save the activation record naming the new selector and snapshot,
/// flip the manifest entry under the manifest coordinator, then enqueue
/// retirement of the outgoing selector.
///
/// The new `entity_inventory_sha256` MUST be recorded before the activation
/// record is saved: the re-staged documents carry snapshot-qualified entity ids
/// derived from the NEW snapshot id, so leaving the old inventory in place would
/// make `collect_preserved_collected_documents` refuse the project on the next
/// full rebuild.
///
/// Crash window, stated rather than hidden: the writer's `delete_all_documents`
/// plus re-adds are still uncommitted here, so a failure after the manifest flip
/// leaves the manifest naming a selector whose documents were never committed.
/// That is self-healing, not a wedge - the next pass observes the flipped
/// selector as `Current` and re-stages it - and it is strictly better than the
/// alternative, since the outgoing selector's documents are already deleted in
/// this writer and the manifest has to move for the committed index to be
/// searchable at all.
#[allow(clippy::too_many_arguments)]
fn migrate_collected_materialization(
    project_id: &str,
    active: &ActiveCollectedSource,
    activation: &bbox_code_source_store::ActivationRecord,
    stored: &bbox_code_source_store::StoredGeneration,
    staged: &CollectedIndexResult,
    store: &bbox_code_source_store::CodeSourceStore,
    edges_dir: &Path,
    stats: &mut ProjectIndexStats,
) -> Result<()> {
    tracing::info!(
        project_id = %project_id,
        generation = %active.generation_id,
        outgoing_selector = %active.selector,
        selector = %staged.selector,
        outgoing_snapshot = %activation.snapshot_id,
        snapshot = %staged.snapshot_id,
        "migrating an active collected generation to the current materialization version"
    );
    store.record_materialization(
        &stored.descriptor.scope,
        &active.generation_id,
        staged.document_count,
        staged.entity_inventory_sha256.clone(),
    )?;
    store.save_activation(&bbox_code_source_store::ActivationRecord {
        version: 1,
        project_id: project_id.to_string(),
        generation_id: active.generation_id.clone(),
        selector: staged.selector.clone(),
        snapshot_id: staged.snapshot_id.clone(),
        document_count: staged.document_count,
        entity_inventory_sha256: staged.entity_inventory_sha256.clone(),
        current_chunk_targets: staged.current_chunk_targets.clone().into_iter().collect(),
        activated_unix_secs: std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs())
            .unwrap_or_default(),
        // Preserved verbatim: a cutback in flight is orthogonal to which
        // materialization version minted the selector, and clearing it here
        // would silently complete somebody else's transition.
        cutback_pending: activation.cutback_pending,
        diagnostic: activation.diagnostic.clone(),
    })?;
    // `repo_id` and `head_commit` are advisory manifest metadata from P3-B on:
    // this flip opens no Git, so neither value gates anything it commits.
    bbox_edge_sidecar::snapshot::activate_collected_snapshot_with(
        edges_dir,
        project_id,
        stored.descriptor.scope.repo_id(),
        &stored.descriptor.head_commit,
        &active.generation_id,
        &staged.selector,
        &staged.snapshot_id,
        || Ok(()),
    )?;
    // Validator-safe since the P3-B retirement fix widened
    // `validate_retirement_record` to the general snapshot-id shape.
    store.enqueue_retirement(&bbox_code_source_store::RetirementRecord {
        version: 1,
        project_id: project_id.to_string(),
        selector: active.selector.clone(),
        snapshot_id: activation.snapshot_id.clone(),
        generation_id: Some(active.generation_id.clone()),
    })?;
    stats
        .migrated_collected_selectors
        .insert(project_id.to_string(), staged.selector.clone());
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn index_active_collected_project(
    identity: &bbox_corpus_core::code_project_identity::CodeProjectIdentity,
    project: Option<&ProjectRecord>,
    git_root: Option<&Path>,
    active: &ActiveCollectedSource,
    store: &bbox_code_source_store::CodeSourceStore,
    f: FieldHandles,
    writer: &mut IndexWriter,
    meta: &mut HashMap<String, FileMeta>,
    edges_dir: &Path,
    git_meta_dir: &Path,
    force_full: bool,
    preserved_documents_are_staged: bool,
    stats: &mut ProjectIndexStats,
) -> Result<()> {
    let project_id = identity.project_id.as_str();
    let repo_id = project.and_then(|project| project.repo_id.as_deref());
    let activation = store
        .load_activation(project_id)?
        .ok_or_else(|| anyhow::anyhow!("active collected selector has no activation record"))?;
    // Unchanged and deliberately BEFORE the classification: an activation
    // record that disagrees with the manifest is inconsistent regardless of
    // which materialization version either was minted under.
    if activation.generation_id != active.generation_id || activation.selector != active.selector {
        anyhow::bail!("active collected selector disagrees with its activation record");
    }
    let materialization = classify_collected_materialization(project_id, active, &activation)?;
    let stored = store.find_generation(&active.generation_id)?;
    let entries = store.load_generation_entries(&stored.descriptor.scope, &active.generation_id)?;
    let blobs_available = !force_full
        || entries.iter().all(|entry| {
            store
                .verified_blob_file(&entry.content_sha256, entry.size)
                .is_ok()
        });
    if force_full && !blobs_available {
        store.mark_generation_state(
            &stored.descriptor.scope,
            &active.generation_id,
            bbox_code_source::GenerationState::MissingBlobData,
            Some("one or more active source blobs are missing or corrupt".to_string()),
        )?;
        store.record_health_failure(
            project_id,
            "missing_blob_data",
            "one or more active source blobs are missing or corrupt",
        )?;
    } else if force_full && stored.state == bbox_code_source::GenerationState::MissingBlobData {
        store.mark_generation_state(
            &stored.descriptor.scope,
            &active.generation_id,
            bbox_code_source::GenerationState::Active,
            None,
        )?;
        store.clear_health_failure(project_id, "missing_blob_data")?;
    }
    let staged = if force_full && blobs_available {
        // Planning supplies the identity (Phase 3 plan section 7 item 1), so
        // a rebuild of a project with zero attachments takes the same
        // identity-first path a fresh activation does and produces
        // byte-identical documents to it.
        Some(stage_collected_project_generation(
            identity,
            ProjectFileCompatFields { repo_id },
            &stored.descriptor,
            &active.generation_id,
            &entries,
            f,
            writer,
            edges_dir,
            &mut stats.publication,
            |entry| {
                let mut file = store.verified_blob_file(&entry.content_sha256, entry.size)?;
                let mut bytes = Vec::with_capacity(entry.size as usize);
                file.read_to_end(&mut bytes)?;
                Ok(bytes)
            },
        )?)
    } else if force_full && !preserved_documents_are_staged {
        anyhow::bail!(
            "active collected generation has unavailable blobs and no verified read-back"
        );
    } else {
        if force_full && !blobs_available {
            tracing::warn!(
                project_id = %project_id,
                generation = %active.generation_id,
                "full rebuild preserved active collected documents because source blobs are unavailable"
            );
        }
        None
    };
    // Materialization migration (Phase 3 plan section 9): the re-stage above
    // already minted the CURRENT selector and snapshot id, so all that is left
    // is to move the durable pointers onto them and retire the outgoing ones.
    //
    // Zero leases and zero Git: the whole arm reads verified store blobs and
    // writes store records plus the edge-sidecar manifest, which is what lets a
    // remote-only project with no attachment migrate at all.
    //
    // An incremental pass never reaches this (`staged` is `None` when
    // `force_full` is false), so it preserves the outgoing documents and the
    // next full rebuild performs the migration.
    if let (CollectedMaterializationState::Outgoing, Some(staged)) = (materialization, &staged) {
        migrate_collected_materialization(
            project_id,
            active,
            &activation,
            &stored,
            staged,
            store,
            edges_dir,
            stats,
        )?;
    }
    let current_chunk_targets = staged
        .as_ref()
        .map(|result| result.current_chunk_targets.clone())
        .unwrap_or_else(|| {
            activation
                .current_chunk_targets
                .clone()
                .into_iter()
                .collect()
        });
    // The Git history walk needs the version-1 record (its own path-bearing
    // lane, untouched this milestone); a project with no attachment reaches
    // the degradation arm below instead, exactly as a denied Git lease does.
    let git_stats = if let (Some(project), Some(git_root)) = (project, git_root) {
        let mut git_ctx = super::git_history::GitIndexContext {
            f,
            writer,
            meta,
            edges_dir,
            git_meta_dir,
            force_full,
            publication: &mut stats.publication,
            project_display: identity.display_name.as_str(),
        };
        let stats = super::git_history::index_git_history_for_project(
            project,
            git_root,
            &current_chunk_targets,
            &mut git_ctx,
        )?;
        if let Err(error) = store.clear_health_failure(project_id, "git_history_unavailable") {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to clear GitHistory degradation record"
            );
        }
        stats
    } else {
        if let Err(error) = store.record_health_failure(
            project_id,
            "git_history_unavailable",
            "active code generation has no validated GitHistory attachment",
        ) {
            tracing::warn!(
                project_id = %project_id,
                error = %error,
                "failed to persist GitHistory degradation record"
            );
        }
        super::git_history::GitIndexStats::default()
    };
    stats.indexed_commits += git_stats.indexed_commits;
    stats.indexed_docs += git_stats.indexed_commits;
    stats.emitted_edges += git_stats.emitted_edges;
    if let Some(staged) = staged {
        stats.publication.stage_snapshot_git_current(
            edges_dir,
            project_id,
            &staged.snapshot_id,
            repo_id.is_some(),
        );
        stats.indexed_docs += staged.document_count;
        stats.indexed_files += stored.descriptor.file_count;
    } else {
        stats.publication.stage_snapshot_git_current(
            edges_dir,
            project_id,
            &activation.snapshot_id,
            repo_id.is_some(),
        );
    }
    Ok(())
}

/// Bridge/local reindex-lane document. `project_display` is the identity's
/// display name (plan section 4.6 / P3-A item 1), never `canonical_path`: the
/// P3-E cut removes the checkout path from `project` on every project-file
/// document, local staging included.
pub fn build_project_file_doc(
    chunk: &Chunk,
    project: &ProjectRecord,
    project_display: &str,
    commit_sha: Option<&str>,
    snapshot_id: Option<&str>,
    f: FieldHandles,
) -> TantivyDocument {
    let selector = bbox_code_source::local_selector(&project.project_id);
    let relative_path = normalized_relative_path(&chunk.file_path);
    let entry_key = bbox_code_source::source_entry_key(&selector, &relative_path);
    build_project_file_doc_for_source(
        chunk,
        &project.project_id,
        project.repo_id.as_deref(),
        &relative_path,
        project_display,
        commit_sha,
        snapshot_id,
        &selector,
        "local",
        &entry_key,
        f,
    )
}

/// Stage one collected generation from immutable source blobs.
///
/// Identity-first (governing section 10.1, Phase 3 plan section 6 item 1):
/// nothing here reads a checkout, so the caller supplies a
/// [`CodeProjectIdentity`] instead of a path-bearing `ProjectRecord`. At the
/// P3-E cut the documents carry `project` = the identity's display name and
/// `file_path`/`relative_path` = the manifest's normalized relative path.
#[allow(clippy::too_many_arguments)]
pub fn stage_collected_project_generation<F>(
    identity: &CodeProjectIdentity,
    compat: ProjectFileCompatFields<'_>,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    publication: &mut ProjectIndexPublicationBundle,
    open_bytes: F,
) -> Result<CollectedIndexResult>
where
    F: FnMut(&bbox_code_source::ManifestEntry) -> Result<Vec<u8>>,
{
    descriptor.validate_manifest(entries, u64::MAX, u64::MAX)?;
    let project_id = identity.project_id.as_str();
    let snapshot_id = bbox_edge_sidecar::snapshot::collected_snapshot_id(project_id, generation_id);
    let selector = collected_materialization_selector(project_id, generation_id);
    stage_project_file_generation(
        identity,
        ProjectFileCompatFields {
            repo_id: compat.repo_id,
        },
        descriptor,
        generation_id,
        entries,
        &selector,
        &snapshot_id,
        false,
        f,
        writer,
        edges_dir,
        publication,
        open_bytes,
    )
}

/// Stage one local generation by walking the leased checkout.
///
/// The caller (the writer actor) holds the validated local-source lease for
/// the whole call and passes its roots; `scope` is the authorized producer
/// scope, which for a catalog identity equals the identity's own published
/// scope and for a bridge identity is the only place a `PublishedScope`
/// exists at all (D-034: a bridge identity never fabricates one).
#[allow(clippy::too_many_arguments)]
pub fn stage_local_project_generation(
    identity: &CodeProjectIdentity,
    compat: ProjectFileCompatFields<'_>,
    scope: &bbox_corpus_core::identity::PublishedScope,
    project_root: &Path,
    git_root: &Path,
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    publication: &mut ProjectIndexPublicationBundle,
) -> Result<CollectedIndexResult> {
    let project_id = identity.project_id.as_str();
    let root = project_root
        .canonicalize()
        .with_context(|| format!("canonicalizing local project {project_id}"))?;
    if !root.is_dir() {
        anyhow::bail!("registered local project root is not a directory");
    }
    let head_commit = bbox_corpus_core::git::current_head(git_root)
        .ok_or_else(|| anyhow::anyhow!("registered local project has no readable Git HEAD"))?;
    let mut scanned = Vec::new();
    let _scan_stats = scan_project_files(&root, &mut scanned)?;
    let mut entries = Vec::with_capacity(scanned.len());
    for (absolute_path, _mtime, declared_size) in scanned {
        let absolute_path = PathBuf::from(absolute_path);
        let relative_path = absolute_path
            .strip_prefix(&root)
            .context("scanned local source escaped its registered root")?
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("local source path is not valid UTF-8"))?
            .replace(std::path::MAIN_SEPARATOR, "/");
        let bytes = read_regular_file_confined(&root, Path::new(&relative_path))
            .with_context(|| format!("reading local source {relative_path}"))?;
        if bytes.len() as u64 != declared_size {
            anyhow::bail!("local source changed while preparing cutback");
        }
        entries.push(bbox_code_source::ManifestEntry {
            relative_path,
            content_sha256: full_hash(&bytes),
            size: declared_size,
        });
    }
    entries.sort_by(|left, right| {
        left.relative_path
            .as_bytes()
            .cmp(right.relative_path.as_bytes())
    });
    let logical_bytes = entries.iter().try_fold(0_u64, |total, entry| {
        total
            .checked_add(entry.size)
            .ok_or_else(|| anyhow::anyhow!("local source byte count overflow"))
    })?;
    let dirty_fingerprint = bbox_code_source::dirty_fingerprint(&head_commit, &entries);
    let descriptor = bbox_code_source::GenerationDescriptor {
        schema_version: bbox_code_source::SCHEMA_VERSION,
        walker_policy_version: bbox_code_source::WALKER_POLICY_VERSION.to_string(),
        scope: scope.clone(),
        head_commit: head_commit.clone(),
        dirty_fingerprint: dirty_fingerprint.clone(),
        manifest_sha256: bbox_code_source::manifest_sha256(&entries),
        file_count: entries.len() as u64,
        logical_bytes,
    };
    descriptor.validate_manifest(&entries, u64::MAX, u64::MAX)?;
    let selector = bbox_code_source::local_selector(project_id);
    let worktree_dirty = bbox_corpus_core::git::is_worktree_dirty(git_root);
    let snapshot_id = if worktree_dirty {
        bbox_edge_sidecar::snapshot::nongit_snapshot_id(project_id, &dirty_fingerprint)
    } else {
        // Bridge local staging keeps head-bound clean snapshots
        // unconditionally this milestone (plan section 4.6); the
        // `legacy_local_snapshot_id` derivation arrives with the catalog
        // local lane.
        bbox_edge_sidecar::snapshot::clean_snapshot_id(scope.repo_id(), project_id, &head_commit)
    };
    stage_project_file_generation(
        identity,
        compat,
        &descriptor,
        "local",
        &entries,
        &selector,
        &snapshot_id,
        worktree_dirty,
        f,
        writer,
        edges_dir,
        publication,
        |entry| {
            read_regular_file_confined(&root, Path::new(&entry.relative_path))
                .with_context(|| format!("re-reading local source {}", entry.relative_path))
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_project_file_generation<F>(
    identity: &CodeProjectIdentity,
    compat: ProjectFileCompatFields<'_>,
    descriptor: &bbox_code_source::GenerationDescriptor,
    generation_id: &str,
    entries: &[bbox_code_source::ManifestEntry],
    selector: &str,
    snapshot_id: &str,
    worktree_dirty: bool,
    f: FieldHandles,
    writer: &mut IndexWriter,
    edges_dir: &Path,
    publication: &mut ProjectIndexPublicationBundle,
    mut open_bytes: F,
) -> Result<CollectedIndexResult>
where
    F: FnMut(&bbox_code_source::ManifestEntry) -> Result<Vec<u8>>,
{
    const MAX_STAGED_SYMBOLS: usize = 2_000_000;
    const MAX_STAGED_CHUNK_TARGETS: usize = 2_000_000;
    const MAX_STAGED_ENTITY_ID_BYTES: usize = 256 * 1024 * 1024;

    let project_id = identity.project_id.as_str();
    // The identity is the single authority for the display value at the P3-E
    // cut: catalog mode carries the catalog display name, bridge mode the
    // first alias else the project id. No checkout root participates.
    let project_display = identity.display_name.as_str();
    let registry = chunker::default_registry();
    let mut chunk_entry = |entry: &bbox_code_source::ManifestEntry| {
        let relative_path = Path::new(&entry.relative_path);
        let bytes = open_bytes(entry)
            .with_context(|| format!("opening collected source {}", entry.relative_path))?;
        if bytes.len() as u64 != entry.size || full_hash(&bytes) != entry.content_sha256 {
            anyhow::bail!("collected source blob failed manifest verification");
        }
        if is_binary(relative_path, &bytes) {
            return Ok(None);
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(relative_path, &bytes[..sniff_len]))
        else {
            return Ok(None);
        };
        let (chunks, edges) = format.chunk(relative_path, &bytes).with_context(|| {
            format!(
                "chunking collected source {} as {}",
                entry.relative_path,
                format.format_id()
            )
        })?;
        let chunks = bound_chunks(&finalize_chunks(project_id, relative_path, chunks));
        Ok(Some((chunks, edges)))
    };

    // Pass one retains only symbol identities. Chunk bodies and file bytes are
    // released after each immutable blob, so generation size no longer maps
    // directly to peak staging memory.
    let mut symbol_table = HashMap::new();
    for entry in entries {
        let Some((chunks, _edges)) = chunk_entry(entry)? else {
            continue;
        };
        extend_symbol_table(&mut symbol_table, &chunks, Some(snapshot_id));
        if symbol_table.len() > MAX_STAGED_SYMBOLS {
            anyhow::bail!("collected source symbol table exceeds the staging safety limit");
        }
    }

    writer.delete_term(Term::from_field_text(f.code_source_selector, selector));

    let mut stats = ProjectIndexStats::default();
    let mut entity_ids = Vec::new();
    let mut entity_id_bytes = 0_usize;
    let mut current_chunk_targets = HashMap::new();
    static STAGED_SNAPSHOT_COUNTER: AtomicU64 = AtomicU64::new(0);
    let staged_filename = format!(
        ".project.publish-pending-{}-{}.jsonl",
        std::process::id(),
        STAGED_SNAPSHOT_COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    let mut edge_writer = bbox_edge_sidecar::snapshot::create_snapshot_edge_writer(
        edges_dir,
        project_id,
        snapshot_id,
        &staged_filename,
    )?;
    for entry in entries {
        let Some((chunks, parser_edges)) = chunk_entry(entry)? else {
            continue;
        };
        let mut project_edges = derive_edges(&chunks, parser_edges, Some(snapshot_id));
        project_edges.extend(derive_code_edges(
            &chunks,
            &symbol_table,
            &mut stats,
            Some(snapshot_id),
        ));
        current_chunk_targets.extend(git_targets_for_scope(
            descriptor.scope.bbox_root_relpath(),
            &chunks,
            Some(snapshot_id),
        ));
        if current_chunk_targets.len() > MAX_STAGED_CHUNK_TARGETS {
            anyhow::bail!("collected source chunk targets exceed the staging safety limit");
        }
        let sidecar_edges = project_edges
            .into_iter()
            .map(|edge| bbox_edge_sidecar::edge_sidecar::Edge {
                source: edge.source,
                kind: edge.kind,
                target: edge.target,
                provenance: edge.provenance,
                confidence: edge.confidence,
                metadata: Default::default(),
            })
            .collect::<Vec<_>>();
        edge_writer.append(&sidecar_edges)?;

        let entry_key = bbox_code_source::source_entry_key(&selector, &entry.relative_path);
        for chunk in chunks {
            let entity_id =
                super::embed_hook::project_file_entity_id_for_snapshot(&chunk, Some(snapshot_id));
            entity_id_bytes = entity_id_bytes.saturating_add(entity_id.len());
            if entity_id_bytes > MAX_STAGED_ENTITY_ID_BYTES {
                anyhow::bail!("collected source entity inventory exceeds the staging safety limit");
            }
            let doc = build_project_file_doc_for_source(
                &chunk,
                project_id,
                compat.repo_id,
                &entry.relative_path,
                project_display,
                Some(&descriptor.head_commit),
                Some(snapshot_id),
                &selector,
                generation_id,
                &entry_key,
                f,
            );
            super::embed_hook::emit_project_file(&chunk, project_display, &entity_id);
            writer.add_document(doc)?;
            entity_ids.push(entity_id);
        }
    }
    edge_writer.finish()?;
    let snapshot_dir =
        bbox_edge_sidecar::snapshot::snapshot_dir(edges_dir, project_id, snapshot_id);
    publication
        .actions
        .push(ProjectIndexPublication::SnapshotRename {
            staged: snapshot_dir.join(&staged_filename),
            destination: snapshot_dir.join("project.jsonl"),
        });

    entity_ids.sort();
    let mut inventory = Sha256::new();
    for entity_id in &entity_ids {
        inventory.update((entity_id.len() as u64).to_be_bytes());
        inventory.update(entity_id.as_bytes());
    }
    Ok(CollectedIndexResult {
        snapshot_id: snapshot_id.to_string(),
        selector: selector.to_string(),
        document_count: entity_ids.len() as u64,
        entity_inventory_sha256: hex::encode(inventory.finalize()),
        current_chunk_targets,
        head_commit: descriptor.head_commit.clone(),
        dirty_fingerprint: descriptor.dirty_fingerprint.clone(),
        worktree_dirty,
    })
}

/// Normalize a chunk-relative path to the slash-separated form the stored
/// `relative_path`/`file_path` fields and the `FileMeta` composite key all
/// use. Chunks already carry a project-relative path
/// ([`finalize_chunks`]); this only fixes the separator on non-slash hosts.
pub fn normalized_relative_path(relative_path: &Path) -> String {
    relative_path
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

#[allow(clippy::too_many_arguments)]
pub fn build_project_file_doc_for_source(
    chunk: &Chunk,
    project_id: &str,
    repo_id: Option<&str>,
    relative_path: &str,
    project_display: &str,
    commit_sha: Option<&str>,
    snapshot_id: Option<&str>,
    selector: &str,
    generation: &str,
    entry_key: &str,
    f: FieldHandles,
) -> TantivyDocument {
    let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(chunk, snapshot_id);
    let mut doc = TantivyDocument::new();
    doc.add_text(f.doc_type, "project_file");
    doc.add_text(f.parser_version, entity_ref::PARSER_VERSION);
    doc.add_text(f.content, &chunk.content);
    if chunk.chunk_kind == "code_block" {
        doc.add_text(f.code_content, &chunk.content);
    }
    doc.add_text(f.session_id, "");
    doc.add_text(f.account, "project_file");
    // P3-E: the display NAME, never a checkout path. Catalog mode supplies the
    // catalog display name; bridge mode supplies the first alias, else the
    // project id (P3-A item 1). Two deliberate search consequences ride this
    // (plan section 4.3 item 2): the permanent literal substring lane stops
    // matching project-file documents by unregistered absolute-path fragments,
    // and BM25 queries carrying host-root components stop matching
    // `path_tokens`. Resolved project filters reach these documents through
    // the `project_id` term lane instead (F7).
    doc.add_text(f.project, project_display);
    doc.add_text(f.role, "file");
    doc.add_text(f.file_path, relative_path);
    doc.add_text(f.relative_path, relative_path);
    doc.add_text(
        f.source_kind,
        bbox_code_source::source_kind_for_selector(selector),
    );
    // An unencodable relative path cannot happen for a chunk that reached
    // here (the walkers and manifest validators reject the shapes
    // `validate_relative_path` rejects), and a document with no `source_uri`
    // is still queryable by every other lane, so this degrades rather than
    // failing the whole pass.
    if let Ok(source_uri) = bbox_code_source::encode_source_uri(project_id, relative_path) {
        doc.add_text(f.source_uri, &source_uri);
    }
    doc.add_text(f.code_source_selector, selector);
    doc.add_text(f.code_source_generation, generation);
    doc.add_text(f.code_source_entry_key, entry_key);
    // Reuse the same string for the tokenized path field; the code tokenizer
    // splits on `/`, `_`, `.`, etc., so src/embed/voyage.rs becomes tokens
    // [src, embed, voyage, rs] available to BM25 ranking.
    doc.add_text(f.path_tokens, relative_path);
    if let Some(symbol) = &chunk.symbol {
        // Symbol path also tokenized for BM25 boost — `Witness.Authority` →
        // [Witness, Authority] so symbol-named queries surface correctly.
        doc.add_text(f.path_tokens, symbol.as_str());
    }
    doc.add_u64(f.byte_offset, chunk.byte_start);
    doc.add_u64(f.byte_end, chunk.byte_end);
    if let Some(line_start) = chunk.line_start {
        doc.add_u64(f.line_start, line_start as u64);
    }
    if let Some(line_end) = chunk.line_end {
        doc.add_u64(f.line_end, line_end as u64);
    }
    doc.add_u64(f.is_subagent, 0);
    doc.add_text(f.project_id, project_id);
    doc.add_text(f.chunk_kind, &chunk.chunk_kind);
    doc.add_text(f.chunk_hash, &chunk.chunk_hash);
    doc.add_text(f.entity_id, &entity_id);
    if let Some(language) = &chunk.language {
        doc.add_text(f.language, language);
    }
    if let Some(symbol) = &chunk.symbol {
        doc.add_text(f.symbol, symbol);
    }
    if let Some(symbol_exact) = &chunk.symbol_exact {
        doc.add_text(f.symbol_exact, symbol_exact);
    }
    if let Some(symbol_kind) = &chunk.symbol_kind {
        doc.add_text(f.symbol_kind, symbol_kind);
    }
    if let Some(parent_kind) = &chunk.parent_kind {
        doc.add_text(f.parent_kind, parent_kind);
    }
    if let Some(repo_id) = repo_id {
        doc.add_text(f.repo_id, repo_id);
    }
    if let Some(commit_sha) = commit_sha {
        doc.add_text(f.commit_sha, commit_sha);
    }
    doc
}

/// Resolve one absolute path inside an already-validated root to the entity
/// ref of the chunk currently covering `byte_range`.
///
/// Identity is the durable project id alone (plan section 4.15): the
/// resolution needs no path-bearing record, and taking one would let a
/// caller's stale `canonical_path` reach a lower crate that must never
/// discover a checkout for itself.
pub fn resolve_current_chunk_entity(
    project_id: &str,
    root: &Path,
    absolute_path: &Path,
    byte_range: Option<(u64, u64)>,
) -> Result<Option<EntityRef>> {
    let relative_path = absolute_path.strip_prefix(root).unwrap_or(absolute_path);
    let bytes = match read_regular_file_confined(root, relative_path) {
        Ok(bytes) => bytes,
        Err(_) => return Ok(None),
    };
    if is_binary(absolute_path, &bytes) {
        return Ok(None);
    }
    let registry = chunker::default_registry();
    let sniff_len = bytes.len().min(4096);
    let Some(format) = registry
        .iter()
        .find(|chunker| chunker.claims(absolute_path, &bytes[..sniff_len]))
    else {
        return Ok(None);
    };
    let (chunks, _edges) = format.chunk(absolute_path, &bytes)?;
    let chunks = bound_chunks(&finalize_chunks(project_id, relative_path, chunks));
    let selected = byte_range
        .and_then(|(start, _end)| {
            chunks
                .iter()
                .find(|chunk| chunk.byte_start <= start && start <= chunk.byte_end)
        })
        .or_else(|| chunks.first());
    Ok(selected.map(|chunk| EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }))
}

#[derive(Debug, PartialEq, Eq)]
enum ProjectFileAction {
    /// mtime+size+materialization version all match — leave as-is.
    Skip,
    /// New file, changed content, or a known-different materialization version
    /// (a real indexer/chunker/parser bump) — must re-chunk.
    Reindex,
}

/// Decide what to do with a scanned project file given its previously indexed
/// metadata. The version dimension forces a re-chunk after an
/// indexer/chunker/parser bump even when the file is byte-for-byte unchanged.
/// An unknown stored version is re-chunked because it cannot prove either the
/// current materialization algorithm or a V2 snapshot identity.
fn classify_project_file(
    prev: Option<&FileMeta>,
    mtime: u64,
    size: u64,
    mat_version: &str,
) -> ProjectFileAction {
    let Some(prev) = prev else {
        return ProjectFileAction::Reindex;
    };
    if prev.mtime != mtime || prev.size != size {
        return ProjectFileAction::Reindex;
    }
    match prev.mat_version.as_deref() {
        Some(v) if v == mat_version => ProjectFileAction::Skip,
        None => ProjectFileAction::Reindex,
        Some(_) => ProjectFileAction::Reindex,
    }
}

fn index_project(
    project: &ProjectRecord,
    root: &Path,
    git_root: Option<&Path>,
    ctx: &mut ProjectIndexContext<'_>,
) -> Result<()> {
    let registry = chunker::default_registry();
    let commit_sha = git_root.and_then(bbox_corpus_core::git::current_head);
    let mut files = Vec::new();
    let mut pending = Vec::new();
    let mut project_edges = Vec::new();
    let scan_stats = scan_project_files(root, &mut files)?;
    ctx.stats.skipped_symlinks += scan_stats.skipped_symlinks;
    ctx.stats.skipped_special += scan_stats.skipped_special;
    ctx.stats.skipped_unsupported += scan_stats.skipped_unsupported;
    ctx.stats.skipped_oversize += scan_stats.skipped_oversize;
    let snapshot_id = ref_snapshot_id(project, root, &files, commit_sha.as_deref());
    let base_mat_version = bbox_edge_sidecar::snapshot::current_materialization_version();
    let mat_version = snapshot_id
        .as_ref()
        .map_or(base_mat_version.clone(), |snapshot_id| {
            format!("{base_mat_version}+ref-snapshot:{snapshot_id}")
        });
    // On-disk freshness-key set for this project, captured before `files` is
    // moved. Keyed by the P3-E composite (plan section 4.6), the same key the
    // meta map now uses, so the deletion detection below compares like with
    // like instead of a mix of absolute and composite keys.
    let selector = bbox_code_source::local_selector(&project.project_id);
    let source_kind = bbox_code_source::source_kind_for_selector(&selector);
    let current_paths: std::collections::HashSet<String> = files
        .iter()
        .map(|(path_str, _, _)| {
            bbox_code_source::project_file_meta_key(
                &project.project_id,
                source_kind,
                &local_relative_path(root, Path::new(path_str)),
            )
        })
        .collect();
    for (path_str, mtime, size) in files {
        let path = PathBuf::from(&path_str);
        let relative_path = local_relative_path(root, &path);
        let meta_key = bbox_code_source::project_file_meta_key(
            &project.project_id,
            source_kind,
            &relative_path,
        );
        match classify_project_file(ctx.meta.get(meta_key.as_str()), mtime, size, &mat_version) {
            ProjectFileAction::Skip => {
                ctx.stats.skipped += 1;
                continue;
            }
            ProjectFileAction::Reindex => {
                let entry_key = bbox_code_source::source_entry_key(&selector, &relative_path);
                ctx.writer.delete_term(Term::from_field_text(
                    ctx.f.code_source_entry_key,
                    &entry_key,
                ));
            }
        }

        let relative_path = PathBuf::from(&relative_path);
        let relative_path = relative_path.as_path();
        let bytes = match read_regular_file_confined(root, relative_path) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "failed to read project file");
                continue;
            }
        };
        if is_binary(&path, &bytes) {
            ctx.stats.skipped += 1;
            continue;
        }
        let sniff_len = bytes.len().min(4096);
        let Some(format) = registry
            .iter()
            .find(|chunker| chunker.claims(&path, &bytes[..sniff_len]))
        else {
            ctx.stats.skipped += 1;
            continue;
        };
        let (chunks, edges) = format
            .chunk(&path, &bytes)
            .with_context(|| format!("chunking {} as {}", path.display(), format.format_id()))?;
        let chunks = finalize_chunks(&project.project_id, relative_path, chunks);
        let bounded_chunks = bound_chunks(&chunks);
        let edges = derive_edges(&bounded_chunks, edges, snapshot_id.as_deref());
        ctx.stats.emitted_edges += edges.len() as u64;
        project_edges.extend(edges);
        pending.push(PendingProjectFile {
            meta_key,
            relative_path: relative_path.to_string_lossy().into_owned(),
            mtime,
            size,
            chunks: bounded_chunks,
        });
    }

    // Captured before `pending` is consumed below. Combined with the git
    // commit count after history indexing, this is the per-project signal that
    // lets `snapshot_after_reindex` skip re-materializing byte-identical edges.
    let files_changed = !pending.is_empty();
    let symbol_table = build_symbol_table(&pending, snapshot_id.as_deref());
    let mut current_chunk_targets = HashMap::new();
    let scope_relpath = git_root
        .and_then(|git_root| bbox_corpus_core::identity::bbox_root_relpath(git_root, root))
        .unwrap_or_else(|| ".".to_string());
    for file in pending {
        let code_edges = derive_code_edges(
            &file.chunks,
            &symbol_table,
            ctx.stats,
            snapshot_id.as_deref(),
        );
        ctx.stats.emitted_edges += code_edges.len() as u64;
        project_edges.extend(code_edges);
        current_chunk_targets.extend(git_targets_for_scope(
            &scope_relpath,
            &file.chunks,
            snapshot_id.as_deref(),
        ));
        for chunk in file.chunks {
            let doc = build_project_file_doc(
                &chunk,
                project,
                ctx.project_display,
                commit_sha.as_deref(),
                snapshot_id.as_deref(),
                ctx.f,
            );
            let entity_id = super::embed_hook::project_file_entity_id_for_snapshot(
                &chunk,
                snapshot_id.as_deref(),
            );
            super::embed_hook::emit_project_file(&chunk, ctx.project_display, &entity_id);
            ctx.writer.add_document(doc)?;
            ctx.stats.indexed_docs += 1;
        }
        ctx.meta.insert(
            file.meta_key.clone(),
            local_file_meta(
                project,
                &file.relative_path,
                file.mtime,
                file.size,
                Some(mat_version.clone()),
            ),
        );
        ctx.stats.indexed_files += 1;
    }
    let git_stats = if let Some(git_root) = git_root {
        let mut git_ctx = super::git_history::GitIndexContext {
            f: ctx.f,
            writer: ctx.writer,
            meta: ctx.meta,
            edges_dir: ctx.edges_dir,
            git_meta_dir: ctx.git_meta_dir,
            force_full: ctx.force_git_full,
            publication: &mut ctx.stats.publication,
            project_display: ctx.project_display,
        };
        super::git_history::index_git_history_for_project(
            project,
            git_root,
            &current_chunk_targets,
            &mut git_ctx,
        )?
    } else {
        super::git_history::GitIndexStats::default()
    };
    ctx.stats.indexed_commits += git_stats.indexed_commits;
    ctx.stats.indexed_docs += git_stats.indexed_commits;
    if git_stats.indexed_commits > 0 {
        ctx.stats.indexed_files += 1;
    }
    ctx.stats.emitted_edges += git_stats.emitted_edges;
    // Purge derived edges for tracked files that were deleted (or are no longer
    // indexable) this pass: present in meta under `root` but absent from the
    // current on-disk scan. The Tantivy docs are purged separately by the
    // reindex deletion sweep; without this, the file's file-anchored edges
    // (NEXT_SECTION / DEFINED_IN / CONTAINS_SYMBOL) survive in the materialized
    // graph. Matched by rel_path_hash, mirroring the incremental-replace
    // granularity; symbol→symbol edges (CALLS/USES_TYPE) carry no file ref and
    // age out with the snapshot id rather than being purged here.
    // P3-E: the relative path is read straight off the composite key instead
    // of being de-fabricated by stripping a checkout root off an absolute one
    // (plan section 4.6, "edge purge reads the relative key directly"). Rows
    // from another project or another lane are filtered out by key shape, so
    // one project's pass can no longer hash a foreign row's path.
    let deleted_rel_hashes: std::collections::HashSet<String> = ctx
        .meta
        .keys()
        .filter(|key| !current_paths.contains(key.as_str()))
        .filter_map(|key| {
            let (key_project_id, _, relative_path) =
                bbox_code_source::parse_project_file_meta_key(key)?;
            (key_project_id == project.project_id).then(|| short_hash(relative_path.as_bytes()))
        })
        .collect();
    let has_deletions = !deleted_rel_hashes.is_empty();
    if !project_edges.is_empty() || has_deletions || ctx.force_git_full {
        ctx.stats
            .publication
            .actions
            .push(ProjectIndexPublication::ProjectEdges {
                edges_dir: ctx.edges_dir.to_path_buf(),
                project_id: project.project_id.clone(),
                edges: project_edges,
                deleted_rel_hashes,
                compact_legacy: ctx.force_git_full,
            });
    }

    let materialization_changed = files_changed || git_stats.indexed_commits > 0 || has_deletions;
    if let Some(git_root) = git_root {
        if let Some(pending) =
            snapshot_after_reindex(project, git_root, ctx.edges_dir, materialization_changed)?
        {
            ctx.stats
                .publication
                .actions
                .push(ProjectIndexPublication::LocalSnapshot(pending));
        }
    }
    Ok(())
}

fn scan_project_files(
    root: &Path,
    out: &mut Vec<(String, u64, u64)>,
) -> Result<ProjectFileScanStats> {
    let mut stats = ProjectFileScanStats::default();
    let walker = WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|entry| entry.depth() == 0 || !is_skipped_entry(entry))
        .build();
    for entry in walker.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        let meta = match fs::symlink_metadata(path) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            stats.skipped_symlinks += 1;
            continue;
        }
        if !meta.is_file() {
            if path != root {
                stats.skipped_special += 1;
            }
            continue;
        }
        let Some(max_bytes) = bbox_code_source::max_bytes_for_path(path) else {
            stats.skipped_unsupported += 1;
            continue;
        };
        if meta.len() > max_bytes {
            stats.skipped_oversize += 1;
            continue;
        }
        let mtime = meta
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs())
            .unwrap_or_default();
        let Some(path) = path.to_str() else {
            stats.skipped_unsupported += 1;
            continue;
        };
        out.push((path.to_string(), mtime, meta.len()));
    }
    Ok(stats)
}

fn is_skipped_entry(entry: &DirEntry) -> bool {
    // `.bbox/` is blackbox's own control directory: project config, MCP wiring,
    // catalog-owned artifacts, and (per the repo-owned-project-state design)
    // structured knowledge owned by a dedicated spooler. It must NOT be pulled
    // into the generic project_file corpus, or its JSON/TOML/MD gets indexed as
    // project source — duplicating catalog/knowledge entities with confusing
    // search hits. Skip it like any other dotdir.
    entry
        .file_name()
        .to_str()
        .is_some_and(bbox_code_source::is_skipped_component)
}

#[cfg(test)]
fn is_supported_text_path(path: &Path) -> bool {
    bbox_code_source::is_supported_source_path(path)
}

#[cfg(unix)]
fn read_regular_file_confined(root: &Path, relative_path: &Path) -> Result<Vec<u8>> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd as _, FromRawFd as _};
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    bbox_code_source::validate_relative_path(
        relative_path
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("source path is not valid UTF-8"))?,
    )?;
    let max_bytes = bbox_code_source::max_bytes_for_path(relative_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported source path"))?;
    let mut options = fs::OpenOptions::new();
    options.read(true);
    options.custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut directory = options.open(root)?;
    let components = relative_path.components().collect::<Vec<_>>();
    for (index, component) in components.iter().enumerate() {
        let std::path::Component::Normal(name) = component else {
            anyhow::bail!("source path has a non-normal component");
        };
        let name = CString::new(name.as_bytes()).context("source path contains NUL")?;
        let last = index + 1 == components.len();
        let flags = if last {
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        } else {
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC
        };
        let fd = unsafe { libc::openat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let opened = unsafe { fs::File::from_raw_fd(fd) };
        if last {
            let metadata = opened.metadata()?;
            if !metadata.is_file() || metadata.len() > max_bytes {
                anyhow::bail!("source is not a regular bounded file");
            }
            let mut bytes = Vec::new();
            opened
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 > max_bytes {
                anyhow::bail!("source exceeds its byte cap");
            }
            return Ok(bytes);
        }
        directory = opened;
    }
    anyhow::bail!("source path is empty")
}

#[cfg(not(unix))]
fn read_regular_file_confined(root: &Path, relative_path: &Path) -> Result<Vec<u8>> {
    let max_bytes = bbox_code_source::max_bytes_for_path(relative_path)
        .ok_or_else(|| anyhow::anyhow!("unsupported source path"))?;
    let path = root.join(relative_path);
    let canonical_parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("source path has no parent"))?
        .canonicalize()?;
    if !canonical_parent.starts_with(root) {
        anyhow::bail!("source path escaped configured root");
    }
    let file = fs::OpenOptions::new().read(true).open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        anyhow::bail!("source is not a regular bounded file");
    }
    let mut bytes = Vec::with_capacity(metadata.len().min(max_bytes) as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max_bytes {
        anyhow::bail!("source exceeds its byte cap");
    }
    Ok(bytes)
}

/// Slash-normalized project-relative path for one scanned absolute path.
/// `unwrap_or(path)` keeps a path that somehow escaped the root verbatim
/// rather than silently rebasing it; the walkers only ever produce paths
/// under `root`.
fn local_relative_path(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .replace(std::path::MAIN_SEPARATOR, "/")
}

fn local_file_meta(
    project: &ProjectRecord,
    relative_path: &str,
    mtime: u64,
    size: u64,
    mat_version: Option<String>,
) -> FileMeta {
    let selector = bbox_code_source::local_selector(&project.project_id);
    let entry_key = bbox_code_source::source_entry_key(&selector, relative_path);
    FileMeta {
        mtime,
        size,
        mat_version,
        source: FileMetaSource::LocalProjectFile {
            project_id: project.project_id.clone(),
            selector,
            relative_path: relative_path.to_string(),
            entry_key,
        },
    }
}

fn finalize_chunks(project_id: &str, rel_path: &Path, chunks: Vec<Chunk>) -> Vec<Chunk> {
    let rel_path_hash = short_hash(rel_path.to_string_lossy().as_bytes());
    chunks
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            let chunk_hash = full_hash(chunk.content.as_bytes());
            chunk.project_id = project_id.to_string();
            chunk.file_path = rel_path.to_path_buf();
            chunk.rel_path_hash.clone_from(&rel_path_hash);
            chunk.chunk_hash = chunk_hash;
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn git_targets_for_scope(
    bbox_root_relpath: &str,
    chunks: &[Chunk],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    super::git_history::current_chunk_targets(chunks, snapshot_id)
        .into_iter()
        .map(|(relative_path, entity)| {
            (
                super::git_history::repo_relative_path_for_scope(bbox_root_relpath, &relative_path),
                entity,
            )
        })
        .collect()
}

fn split_oversized_chunk(chunk: &Chunk) -> Vec<Chunk> {
    if chunk.content.len() <= chunker::MAX_CHUNK_BYTES {
        return vec![chunk.clone()];
    }
    let mut out = Vec::new();
    let mut start = 0usize;
    while start < chunk.content.len() {
        let mut end = (start + chunker::MAX_CHUNK_BYTES).min(chunk.content.len());
        while !chunk.content.is_char_boundary(end) {
            end -= 1;
        }
        let content = chunk.content[start..end].to_string();
        let mut split = chunk.clone();
        split.content = content;
        split.byte_start = chunk.byte_start + start as u64;
        split.byte_end = chunk.byte_start + end as u64;
        split.chunk_hash = full_hash(split.content.as_bytes());
        split.occurrence_idx = out.len() as u32;
        out.push(split);
        start = end;
    }
    out
}

fn bound_chunks(chunks: &[Chunk]) -> Vec<Chunk> {
    chunks
        .iter()
        .flat_map(split_oversized_chunk)
        .enumerate()
        .map(|(idx, mut chunk)| {
            chunk.occurrence_idx = idx as u32;
            chunk
        })
        .collect()
}

fn derive_edges(chunks: &[Chunk], mut edges: Vec<Edge>, snapshot_id: Option<&str>) -> Vec<Edge> {
    for pair in chunks.windows(2) {
        edges.push(Edge {
            source: chunk_ref(&pair[0], snapshot_id),
            kind: "NEXT_SECTION".to_string(),
            target: chunk_ref(&pair[1], snapshot_id),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
        });
    }
    // Markdown file/section link extraction is separate from storage lifecycle:
    // this pass indexes current project-file chunks and code-symbol edges.
    edges
}

fn build_symbol_table(
    files: &[PendingProjectFile],
    snapshot_id: Option<&str>,
) -> HashMap<String, EntityRef> {
    let mut symbols = HashMap::new();
    for file in files {
        extend_symbol_table(&mut symbols, &file.chunks, snapshot_id);
    }
    symbols
}

fn extend_symbol_table(
    symbols: &mut HashMap<String, EntityRef>,
    chunks: &[Chunk],
    snapshot_id: Option<&str>,
) {
    for chunk in chunks {
        if chunk.chunk_kind != "code_block" {
            continue;
        }
        let Some(qualified_name) = &chunk.symbol else {
            continue;
        };
        let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
        symbols
            .entry(qualified_name.clone())
            .or_insert(symbol.clone());
        if let Some(bare) = &chunk.symbol_exact {
            symbols.entry(bare.clone()).or_insert(symbol);
        }
    }
}

fn derive_code_edges(
    chunks: &[Chunk],
    symbols: &HashMap<String, EntityRef>,
    stats: &mut ProjectIndexStats,
    snapshot_id: Option<&str>,
) -> Vec<Edge> {
    let mut edges = Vec::new();
    for chunk in chunks
        .iter()
        .filter(|chunk| chunk.chunk_kind == "code_block")
    {
        let file_ref = chunk_ref(chunk, snapshot_id);
        if let Some(qualified_name) = &chunk.symbol {
            let symbol = symbol_ref(chunk, qualified_name, snapshot_id);
            edges.push(edge(
                symbol.clone(),
                "DEFINED_IN",
                file_ref.clone(),
                EdgeConfidence::Exact,
            ));
            edges.push(edge(
                file_ref.clone(),
                "CONTAINS_SYMBOL",
                symbol.clone(),
                EdgeConfidence::Exact,
            ));
            edges.extend(derive_has_field_edges(chunk, &symbol, symbols));
            edges.extend(derive_impl_trait_edges(chunk, &symbol, symbols));
            for callee in call_names(&chunk.content) {
                if let Some(target) = symbols.get(&callee) {
                    edges.push(edge(
                        symbol.clone(),
                        "CALLS",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                    stats.call_edges += 1;
                    stats.resolved_call_edges += 1;
                }
            }
            for type_name in type_names(&chunk.content) {
                if let Some(target) = symbols.get(&type_name) {
                    edges.push(edge(
                        symbol.clone(),
                        "USES_TYPE",
                        target.clone(),
                        EdgeConfidence::Heuristic,
                    ));
                }
            }
        }
    }
    edges
}

fn edge(source: EntityRef, kind: &str, target: EntityRef, confidence: EdgeConfidence) -> Edge {
    Edge {
        source,
        kind: kind.to_string(),
        target,
        provenance: EdgeProvenance::Derived,
        confidence,
    }
}

fn symbol_ref(chunk: &Chunk, qualified_name: &str, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::SymbolV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            qualified_name: qualified_name.to_string(),
            defn_hash: chunk.chunk_hash.clone(),
        };
    }
    EntityRef::Symbol {
        project_id: chunk.project_id.clone(),
        qualified_name: qualified_name.to_string(),
        defn_hash: chunk.chunk_hash.clone(),
    }
}

fn resolve_symbol<'a>(
    symbols: &'a HashMap<String, EntityRef>,
    name: &str,
) -> Option<&'a EntityRef> {
    symbols.get(name).or_else(|| {
        name.rsplit_once("::")
            .and_then(|(_, bare)| symbols.get(bare))
            .or_else(|| {
                name.rsplit_once('.')
                    .and_then(|(_, bare)| symbols.get(bare))
            })
    })
}

fn derive_has_field_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let Some(struct_name) = &chunk.symbol else {
        return Vec::new();
    };
    if !chunk.content.contains("struct ") {
        return Vec::new();
    }
    field_names(&chunk.content)
        .into_iter()
        .filter_map(|field| {
            let target = resolve_symbol(symbols, &format!("{struct_name}::{field}"))?;
            Some(edge(
                source.clone(),
                "HAS_FIELD",
                target.clone(),
                EdgeConfidence::Heuristic,
            ))
        })
        .collect()
}

fn derive_impl_trait_edges(
    chunk: &Chunk,
    source: &EntityRef,
    symbols: &HashMap<String, EntityRef>,
) -> Vec<Edge> {
    let header = chunk.content.split('{').next().unwrap_or_default().trim();
    let Some(rest) = header.strip_prefix("impl ") else {
        return Vec::new();
    };
    let Some((trait_name, _target)) = rest.split_once(" for ") else {
        return Vec::new();
    };
    let Some(target) = resolve_symbol(symbols, trait_name.trim()) else {
        return Vec::new();
    };
    vec![edge(
        source.clone(),
        "IMPLEMENTS_TRAIT",
        target.clone(),
        EdgeConfidence::Heuristic,
    )]
}

fn call_names(content: &str) -> Vec<String> {
    let call_pattern = regex::Regex::new(r"\b([A-Za-z_][A-Za-z0-9_]*)\s*\(").unwrap();
    call_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str()))
        .filter(|name| !CALL_KEYWORDS.contains(name))
        .map(str::to_string)
        .collect()
}

fn type_names(content: &str) -> Vec<String> {
    let type_pattern = regex::Regex::new(r"\b([A-Z][A-Za-z0-9_]{2,})\b").unwrap();
    type_pattern
        .captures_iter(content)
        .filter_map(|capture| capture.get(1).map(|name| name.as_str().to_string()))
        .collect()
}

fn field_names(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || !trimmed.contains(':') {
                return None;
            }
            let left = trimmed.split(':').next()?.trim();
            let name = left.split_whitespace().last()?.trim_start_matches("pub ");
            if name
                .chars()
                .all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
            {
                Some(name.to_string())
            } else {
                None
            }
        })
        .collect()
}

const CALL_KEYWORDS: &[&str] = &[
    "as",
    "assert",
    "async",
    "await",
    "break",
    "case",
    "catch",
    "const",
    "continue",
    "def",
    "defer",
    "delete",
    "do",
    "else",
    "finally",
    "fn",
    "for",
    "from",
    "function",
    "go",
    "if",
    "import",
    "in",
    "instanceof",
    "is",
    "lambda",
    "let",
    "loop",
    "match",
    "nameof",
    "new",
    "of",
    "raise",
    "return",
    "select",
    "sizeof",
    "switch",
    "then",
    "throw",
    "try",
    "typeof",
    "unless",
    "using",
    "var",
    "when",
    "where",
    "while",
    "with",
    "yield",
];

fn chunk_ref(chunk: &Chunk, snapshot_id: Option<&str>) -> EntityRef {
    if let Some(snapshot_id) = snapshot_id {
        return EntityRef::ProjectFileV2 {
            project_id: chunk.project_id.clone(),
            snapshot_id: snapshot_id.to_string(),
            rel_path_hash: chunk.rel_path_hash.clone(),
            chunk_hash: chunk.chunk_hash.clone(),
            occurrence_idx: chunk.occurrence_idx,
        };
    }
    EntityRef::ProjectFile {
        project_id: chunk.project_id.clone(),
        rel_path_hash: chunk.rel_path_hash.clone(),
        chunk_hash: chunk.chunk_hash.clone(),
        occurrence_idx: chunk.occurrence_idx,
    }
}

/// PDFs and spreadsheet workbooks are legitimately binary (embedded
/// streams, xref/trailer binary markers, font/image data for PDF; ZIP
/// central-directory/OLE2 CFB structure and compressed part data for
/// xlsx/xlsm/xlam/xlsb/xls/ods) and are expected to contain NUL bytes in
/// their first 4096 bytes; the null-byte heuristic below would otherwise
/// exclude nearly every real-world file of these formats before it ever
/// reaches the chunker registry's own `claims()` (magic-header) check.
/// `PdfChunker::claims` (crates/bbox-chunker/src/pdf.rs) and
/// `XlsxChunker::claims` (crates/bbox-chunker/src/xlsx.rs) are the real
/// gates for whether such a file's content is extractable, so the blanket
/// binary sniff is bypassed by extension here rather than tightened
/// generically. Raster images (X-IMG) are exempted for the same reason:
/// they are byte-diverse compressed binary by construction, and
/// `XImgChunker::claims` (crates/bbox-chunker/src/ximg.rs) is the real
/// gate (extension + magic-byte scan).
fn is_binary(path: &Path, bytes: &[u8]) -> bool {
    if matches!(
        path.extension().and_then(|ext| ext.to_str()),
        Some(
            "pdf"
                | "xlsx"
                | "xlsm"
                | "xlam"
                | "xlsb"
                | "xls"
                | "ods"
                | "docx"
                | "pptx"
                | "png"
                | "jpg"
                | "jpeg"
                | "gif"
                | "webp"
        )
    ) {
        return false;
    }
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

fn full_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

fn short_hash(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    hex::encode(&digest[..4])
}

/// Returns true iff the on-disk materialization for `project_id` already
/// reflects the current HEAD, indexer/chunker version, and worktree dirty state
/// — i.e. re-running `switch_to_*` would reproduce byte-identical edges and only
/// churn mtimes. Any inconsistency (cold start, version bump, branch switch,
/// dirty↔clean drift, GC'd active path) returns false so we materialize as today.
///
/// Gates on the `ManifestIndex` (the loader authority via
/// `active_materialized_paths`), not `WorkspaceManifest`, whose `dirty`/
/// `dirty_fingerprint` fields have no runtime reader and may drift on
/// metadata-only changes (`git add`, same-HEAD branch relabel).
fn materialization_is_current(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    worktree_dirty: bool,
) -> bool {
    let Ok(idx) = bbox_edge_sidecar::manifest::ManifestIndex::load(edges_dir) else {
        // No manifest-index yet (cold start / never materialized) ⇒ materialize.
        return false;
    };
    let Some(entry) = idx.workspaces.get(project_id) else {
        return false;
    };

    // Version-aware expected snapshot for the current HEAD. `clean_snapshot_id`
    // folds INDEXER_VERSION/CHUNKER_VERSION, so a version bump with unchanged
    // mtimes yields a different id ⇒ mismatch ⇒ not skipped.
    let expected_snap =
        bbox_edge_sidecar::snapshot::clean_snapshot_id(repo_id, project_id, head_sha);
    let expected_snap_rel =
        bbox_edge_sidecar::snapshot::active_snapshot_rel(project_id, &expected_snap);
    if entry.active_snapshot.as_deref() != Some(expected_snap_rel.as_str()) {
        return false;
    }

    // Dirty-state consistency across three sources: current worktree, the
    // ManifestIndex overlay pointer, and the overlay dir on disk. Any
    // disagreement forces re-materialization (e.g. a clean checkout that left a
    // stale overlay, which `switch_to_clean_snapshot` must clear).
    let overlay_rel = bbox_edge_sidecar::snapshot::dirty_overlay_rel(project_id);
    let overlay_in_manifest = entry.dirty_overlay.as_deref() == Some(overlay_rel.as_str());
    let overlay_on_disk =
        bbox_edge_sidecar::snapshot::dirty_overlay_dir(edges_dir, project_id).is_dir();
    if worktree_dirty {
        if !overlay_in_manifest || !overlay_on_disk {
            return false;
        }
    } else {
        if entry.dirty_overlay.is_some() || overlay_on_disk {
            return false;
        }
        // Clean ⇒ the active snapshot dir is what the loader reads; it must
        // exist. `active_materialized_paths` silently drops missing dirs, so a
        // GC between passes would lose this project from the graph if we skipped.
        if !bbox_edge_sidecar::snapshot::snapshot_dir(edges_dir, project_id, &expected_snap)
            .is_dir()
        {
            return false;
        }
    }

    true
}

fn snapshot_after_reindex(
    project: &ProjectRecord,
    root: &Path,
    edges_dir: &Path,
    materialization_changed: bool,
) -> Result<Option<LocalSnapshotPublication>> {
    let Some(repo_id) = project.repo_id.as_deref() else {
        return Ok(None);
    };
    let Some(head_sha) = bbox_corpus_core::git::current_head(root) else {
        return Ok(None);
    };
    let branch = bbox_corpus_core::git::current_branch(root);
    let worktree_dirty = bbox_corpus_core::git::is_worktree_dirty(root);

    // Writer-side materialization idempotency. Re-running `switch_to_*` rewrites
    // the dirty overlay via temp-dir + atomic rename, which stamps fresh mtimes
    // on `dirty-current/*.jsonl`. The edge-index rebuild watcher sums sidecar
    // mtimes, so a byte-identical re-materialization still trips a full 18-21s
    // EdgeIndex rebuild. When this pass changed nothing for the project and the
    // on-disk materialization already matches the current head/version/worktree
    // state, skip it. Correctness rests on: derived overlay/snapshot edge content
    // is a deterministic function of (head_sha, changed-file set + contents). No
    // re-chunked file (empty `pending`) and no indexed commit ⇒ identical edges.
    if !materialization_changed
        && materialization_is_current(
            edges_dir,
            &project.project_id,
            repo_id,
            &head_sha,
            worktree_dirty,
        )
    {
        return Ok(None);
    }

    let (dirty_fingerprint, snapshot_id) = if worktree_dirty {
        let fingerprint = bbox_corpus_core::git::dirty_fingerprint(root).unwrap_or_default();
        let snapshot_id =
            bbox_edge_sidecar::snapshot::nongit_snapshot_id(&project.project_id, &fingerprint);
        (Some(fingerprint), snapshot_id)
    } else {
        (
            None,
            bbox_edge_sidecar::snapshot::clean_snapshot_id(repo_id, &project.project_id, &head_sha),
        )
    };
    Ok(Some(LocalSnapshotPublication {
        edges_dir: edges_dir.to_path_buf(),
        project_id: project.project_id.clone(),
        repo_id: repo_id.to_string(),
        branch,
        head_sha,
        dirty: worktree_dirty,
        dirty_fingerprint,
        snapshot_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::build_schema;
    use bbox_chunker::SourceFormatChunker;
    use tantivy::schema::Field;

    #[test]
    fn publication_bundle_rolls_back_earlier_snapshot_when_later_action_fails() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let mut bundle = ProjectIndexPublicationBundle::default();
        bundle.stage_snapshot_git_current(&edges_dir, "p1", "snapshot-a", false);
        bundle.stage_snapshot_git_current(&edges_dir, "../invalid", "snapshot-b", false);

        assert!(bundle.publish().is_err());
        let txn_dir =
            bbox_edge_sidecar::manifest::materialized_dir(&edges_dir).join("workspace/p1/txn");
        if txn_dir.exists() {
            assert_eq!(
                fs::read_dir(txn_dir)
                    .unwrap()
                    .collect::<std::io::Result<Vec<_>>>()
                    .unwrap()
                    .len(),
                0,
                "the first staged transaction must remain under rollback ownership"
            );
        }
    }

    #[test]
    fn document_containers_get_the_larger_byte_budget() {
        use std::path::Path;
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("deck.pdf")),
            Some(bbox_code_source::MAX_DOCUMENT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("Board.DOCX")),
            Some(bbox_code_source::MAX_DOCUMENT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("main.rs")),
            Some(bbox_code_source::MAX_TEXT_FILE_BYTES)
        );
        assert_eq!(
            bbox_code_source::max_bytes_for_path(Path::new("notes.md")),
            Some(bbox_code_source::MAX_TEXT_FILE_BYTES)
        );
    }

    #[test]
    fn images_get_the_provider_capped_byte_budget() {
        use std::path::Path;
        for name in [
            "figure.png",
            "shot.JPG",
            "photo.jpeg",
            "anim.gif",
            "icon.webp",
        ] {
            assert_eq!(
                bbox_code_source::max_bytes_for_path(Path::new(name)),
                Some(bbox_code_source::MAX_IMAGE_FILE_BYTES),
                "{name}"
            );
        }
    }

    #[test]
    fn images_are_supported_and_exempted_from_the_binary_sniff() {
        use std::path::Path;
        assert!(is_supported_text_path(Path::new("figure.png")));
        assert!(is_supported_text_path(Path::new("shot.jpeg")));
        // Real image bytes are byte-diverse binary (NUL bytes included);
        // the extension bypass must apply even though the content sniff
        // alone would call it binary.
        assert!(!is_binary(Path::new("figure.png"), &[0u8; 16]));
        // A non-image file with the same NUL-heavy content is still binary.
        assert!(is_binary(Path::new("figure.bin"), &[0u8; 16]));
    }

    #[test]
    fn project_file_doc_includes_agentic_fields() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("design/agentic-corpus.md"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "doc_section".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("md".into()),
            symbol: None,
            symbol_exact: None,
            symbol_kind: None,
            parent_kind: None,
            line_start: None,
            line_end: None,
            content: "agentic-corpus design".into(),
            byte_start: 10,
            byte_end: 32,
            visual_payload: None,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            "acme-design",
            Some(commit_sha.as_str()),
            None,
            fields,
        );

        assert_eq!(first_text(&doc, fields.doc_type), "project_file");
        assert_eq!(first_text(&doc, fields.chunk_kind), "doc_section");
        assert_eq!(first_text(&doc, fields.language), "md");
        assert_eq!(first_text(&doc, fields.repo_id), "repo1234");
        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!("project_file:proj1234:abcd1234:{}:0", "f".repeat(64))
        );
    }

    /// P3-E: there is no display-root join left on any project-file lane. The
    /// normalizer only fixes the separator, and a path that is already
    /// slash-separated passes through byte-identically.
    #[test]
    fn relative_path_normalization_never_joins_a_host_root() {
        assert_eq!(
            normalized_relative_path(Path::new("src/lib.rs")),
            "src/lib.rs"
        );
        assert_eq!(
            normalized_relative_path(Path::new("a/b/c.md")),
            "a/b/c.md",
            "a nested relative path keeps every component and gains no prefix"
        );
    }

    /// Bridge parity pin for the enumerated document-field change (plan
    /// section 4.3 item 2, step TWO: the P3-E cut). Collected and local
    /// documents are now identical in the once-divergent fields: `project` is
    /// the display name and `file_path` / `relative_path` / `path_tokens` are
    /// the normalized relative path on BOTH lanes. Only `source_kind` and the
    /// selector/generation differ, and every other field is untouched,
    /// `repo_id` included.
    #[test]
    fn collected_and_local_documents_carry_identical_path_free_fields() {
        let (_schema, fields) = build_schema();
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 0,
            language: Some("rust".into()),
            symbol: None,
            symbol_exact: None,
            symbol_kind: None,
            parent_kind: None,
            line_start: None,
            line_end: None,
            content: "fn helper() {}".into(),
            byte_start: 0,
            byte_end: 14,
            visual_payload: None,
        };

        let collected = build_project_file_doc_for_source(
            &chunk,
            "proj1234",
            Some("repo1234"),
            "src/lib.rs",
            "acme",
            None,
            Some("collected-0123456789abcdef"),
            "collected:proj1234:gen",
            "gen",
            "entry-key",
            fields,
        );
        let local = build_project_file_doc_for_source(
            &chunk,
            "proj1234",
            Some("repo1234"),
            "src/lib.rs",
            "acme",
            None,
            Some("head-repo1234-0123456789ab"),
            "local:proj1234",
            "local",
            "entry-key",
            fields,
        );

        for (label, doc) in [("collected", &collected), ("local", &local)] {
            assert_eq!(first_text(doc, fields.project), "acme", "{label} project");
            assert_eq!(
                first_text(doc, fields.file_path),
                "src/lib.rs",
                "{label} file_path"
            );
            assert_eq!(
                first_text(doc, fields.relative_path),
                "src/lib.rs",
                "{label} relative_path"
            );
            assert_eq!(
                first_text(doc, fields.path_tokens),
                "src/lib.rs",
                "{label} path_tokens"
            );
            assert_eq!(
                first_text(doc, fields.source_uri),
                "bbox://project/proj1234/src/lib.rs",
                "{label} source_uri"
            );
            assert_eq!(first_text(doc, fields.project_id), "proj1234");
            assert_eq!(first_text(doc, fields.repo_id), "repo1234");
        }
        assert_eq!(first_text(&collected, fields.source_kind), "collected");
        assert_eq!(first_text(&local, fields.source_kind), "local");
    }

    #[test]
    fn project_file_doc_can_emit_snapshot_specific_entity_id() {
        let (_schema, fields) = build_schema();
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunk = Chunk {
            project_id: "proj1234".into(),
            file_path: PathBuf::from("src/lib.rs"),
            rel_path_hash: "abcd1234".into(),
            chunk_kind: "code_block".into(),
            chunk_hash: "f".repeat(64),
            occurrence_idx: 3,
            language: Some("rust".into()),
            symbol: Some("helper".into()),
            symbol_exact: Some("crate::helper".into()),
            symbol_kind: Some("function".into()),
            parent_kind: None,
            line_start: Some(1),
            line_end: Some(4),
            content: "fn helper() {}".into(),
            byte_start: 0,
            byte_end: 14,
            visual_payload: None,
        };

        let commit_sha = "a".repeat(40);
        let doc = build_project_file_doc(
            &chunk,
            &project,
            "acme",
            Some(commit_sha.as_str()),
            Some("head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"),
            fields,
        );

        assert_eq!(
            first_text(&doc, fields.entity_id),
            format!(
                "project_file_v2:proj1234:head-repo1234-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa:abcd1234:{}:3",
                "f".repeat(64)
            )
        );
    }

    #[test]
    fn tier_a_call_edges_resolve_against_symbol_table() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project.project_id,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn helper() {}",
                    0,
                    14,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "fn caller() { helper(); }",
                    15,
                    39,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("helper".into());
                chunk.symbol_exact = Some("helper".into());
            } else {
                chunk.symbol = Some("caller".into());
                chunk.symbol_exact = Some("caller".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            meta_key: bbox_code_source::project_file_meta_key(
                "proj1234",
                bbox_code_source::SOURCE_KIND_LOCAL,
                "src/lib.rs",
            ),
            relative_path: "src/lib.rs".into(),
            mtime: 1,
            size: 39,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);
        assert!(edges.iter().any(|edge| edge.kind == "CALLS"));
        assert!(stats.call_edges >= 1);
        assert_eq!(stats.resolved_call_edges, stats.call_edges);
    }

    #[test]
    fn tier_a_edges_skip_external_symbol_targets() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let chunks = finalize_chunks(
            &project.project_id,
            Path::new("src/lib.rs"),
            vec![
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "trait LocalTrait {}",
                    0,
                    19,
                    0,
                ),
                bbox_chunker::placeholder_chunk(
                    Path::new("src/lib.rs"),
                    "code_block",
                    Some("rust"),
                    "impl LocalTrait for Thing {}\nuse std::fmt::Display;",
                    20,
                    72,
                    1,
                ),
            ],
        )
        .into_iter()
        .enumerate()
        .map(|(idx, mut chunk)| {
            if idx == 0 {
                chunk.symbol = Some("LocalTrait".into());
                chunk.symbol_exact = Some("LocalTrait".into());
            } else {
                chunk.symbol = Some("Thing::impl".into());
                chunk.symbol_exact = Some("impl".into());
            }
            chunk
        })
        .collect::<Vec<_>>();
        let pending = vec![PendingProjectFile {
            meta_key: bbox_code_source::project_file_meta_key(
                "proj1234",
                bbox_code_source::SOURCE_KIND_LOCAL,
                "src/lib.rs",
            ),
            relative_path: "src/lib.rs".into(),
            mtime: 1,
            size: 72,
            chunks,
        }];
        let symbols = build_symbol_table(&pending, None);
        let mut stats = ProjectIndexStats::default();
        let edges = derive_code_edges(&pending[0].chunks, &symbols, &mut stats, None);

        assert!(edges.iter().any(|edge| edge.kind == "IMPLEMENTS_TRAIT"));
        assert!(!edges.iter().any(|edge| edge.kind == "IMPORTS"));
    }

    #[test]
    fn call_names_skip_flow_control_keywords() {
        let names = call_names("if (cond) { foo(); }");

        assert!(!names.iter().any(|name| name == "if"));
        assert!(names.iter().any(|name| name == "foo"));
    }

    #[test]
    fn json_chunk_hashes_survive_noncanonical_formatting() {
        let project = ProjectRecord {
            project_id: "proj1234".into(),
            repo_id: Some("repo1234".into()),
            canonical_path: "/tmp/repo".into(),
            registered_at: "2026-05-05T17:30:00Z".into(),
            is_git_repo: true,
            languages: Default::default(),
            aliases: Default::default(),
        };
        let left = br#"
        {
          "b": 2,
          "a": { "z": true }
        }
        "#;
        let right = br#"{"a":{"z":true},"b":2}"#;

        let left_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), left)
            .unwrap()
            .0;
        let right_chunks = bbox_chunker::config::JsonChunker
            .chunk(Path::new("config.json"), right)
            .unwrap()
            .0;
        let left_chunks =
            finalize_chunks(&project.project_id, Path::new("config.json"), left_chunks);
        let right_chunks =
            finalize_chunks(&project.project_id, Path::new("config.json"), right_chunks);
        let left_hashes = left_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();
        let right_hashes = right_chunks
            .iter()
            .map(|chunk| (chunk.content.clone(), chunk.chunk_hash.clone()))
            .collect::<Vec<_>>();

        assert_eq!(left_hashes, right_hashes);
    }

    fn first_text(doc: &TantivyDocument, field: Field) -> String {
        doc.get_all(field)
            .next()
            .and_then(|v| match v {
                tantivy::schema::OwnedValue::Str(s) => Some(s.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }

    // --- materialization idempotency guard (issue #2 follow-up) ---
    //
    // These exercise `materialization_is_current`, the decision behind skipping a
    // no-op `snapshot_after_reindex`. Skipping when it returns true is what keeps
    // a byte-identical re-materialization from re-stamping overlay mtimes and
    // tripping the edge-index rebuild watcher; the "force" cases guard against
    // skipping when the on-disk graph would actually go stale.

    const MAT_REPO: &str = "repo-mat";
    const MAT_PROJ: &str = "proj-mat";
    const MAT_HEAD: &str = "abc123def456";

    fn mat_edge(id: &str, target: &str) -> bbox_edge_sidecar::edge_sidecar::Edge {
        bbox_edge_sidecar::edge_sidecar::Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: "DESCRIBES".into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        }
    }

    fn seed_clean(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            vec![mat_edge("k1", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    fn seed_dirty(edges_dir: &Path) {
        bbox_edge_sidecar::snapshot::switch_to_dirty_overlay(
            edges_dir,
            MAT_PROJ,
            MAT_REPO,
            Some("main"),
            MAT_HEAD,
            "fp-dirty",
            vec![mat_edge("k_dirty", "k2")],
            vec![],
            vec![],
        )
        .unwrap();
    }

    #[test]
    fn classify_project_file_covers_skip_and_reindex() {
        let v = bbox_edge_sidecar::snapshot::current_materialization_version();
        let current = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some(v.clone()),
            source: FileMetaSource::LegacyFilesystem,
        };
        // Fully current → skip.
        assert_eq!(
            classify_project_file(Some(&current), 100, 200, &v),
            ProjectFileAction::Skip
        );
        // mtime or size drift → reindex.
        assert_eq!(
            classify_project_file(Some(&current), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        assert_eq!(
            classify_project_file(Some(&current), 100, 201, &v),
            ProjectFileAction::Reindex
        );
        // Known-different version with identical content → reindex (real bump).
        let stale = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: Some("older-version".into()),
            source: FileMetaSource::LegacyFilesystem,
        };
        assert_eq!(
            classify_project_file(Some(&stale), 100, 200, &v),
            ProjectFileAction::Reindex
        );
        // Unknown stored version cannot prove the current materialization.
        let legacy = FileMeta {
            mtime: 100,
            size: 200,
            mat_version: None,
            source: FileMetaSource::LegacyFilesystem,
        };
        assert_eq!(
            classify_project_file(Some(&legacy), 100, 200, &v),
            ProjectFileAction::Reindex
        );
        // Unknown version but content drift → reindex (content wins).
        assert_eq!(
            classify_project_file(Some(&legacy), 101, 200, &v),
            ProjectFileAction::Reindex
        );
        // Never-seen file → reindex.
        assert_eq!(
            classify_project_file(None, 100, 200, &v),
            ProjectFileAction::Reindex
        );
    }

    #[test]
    fn materialization_cold_start_forces_rematerialize() {
        let dir = tempfile::tempdir().unwrap();
        // No manifest-index on disk yet (never materialized).
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_clean_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_steady_state_skips() {
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_head_or_version_change_forces_rematerialize() {
        // A different HEAD — and equivalently an INDEXER/CHUNKER_VERSION bump,
        // since both feed the hashed snapshot_id — must re-materialize even when
        // file mtimes are unchanged.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            "deadbeefcafe",
            false
        ));
    }

    #[test]
    fn materialization_clean_with_stale_overlay_forces_rematerialize() {
        // Worktree is clean now but a dirty overlay is still active; the clean
        // switch must run to clear it, so skipping would leave the stale overlay.
        let dir = tempfile::tempdir().unwrap();
        seed_dirty(dir.path());
        assert!(bbox_edge_sidecar::snapshot::dirty_overlay_dir(dir.path(), MAT_PROJ).is_dir());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn materialization_dirty_without_overlay_forces_rematerialize() {
        // Worktree just went dirty but only a clean snapshot is materialized.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            true
        ));
    }

    #[test]
    fn materialization_missing_active_snapshot_dir_forces_rematerialize() {
        // Manifest still references a snapshot dir that has been GC'd off disk.
        // active_materialized_paths drops missing dirs silently, so re-materialize.
        let dir = tempfile::tempdir().unwrap();
        seed_clean(dir.path());
        let snap_id = bbox_edge_sidecar::snapshot::clean_snapshot_id(MAT_REPO, MAT_PROJ, MAT_HEAD);
        let snap = bbox_edge_sidecar::snapshot::snapshot_dir(dir.path(), MAT_PROJ, &snap_id);
        std::fs::remove_dir_all(&snap).unwrap();
        assert!(!materialization_is_current(
            dir.path(),
            MAT_PROJ,
            MAT_REPO,
            MAT_HEAD,
            false
        ));
    }

    #[test]
    fn scan_skips_bbox_control_dir() {
        use std::fs;
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();

        // Normal project source — must be indexed.
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        fs::write(root.join("README.md"), "# project").unwrap();

        // blackbox control dir — must NOT be indexed (config, MCP wiring,
        // catalog-owned artifacts, and future structured knowledge live here,
        // owned by other subsystems).
        fs::create_dir_all(root.join(".bbox/knowledge")).unwrap();
        fs::write(root.join(".bbox/config.toml"), "x = 1").unwrap();
        fs::write(root.join(".bbox/mcp.json"), "{}").unwrap();
        fs::write(root.join(".bbox/knowledge/entry.json"), "{}").unwrap();

        // Another dotdir — already skipped; sanity anchor.
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/config"), "[core]").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();

        assert!(
            indexed.iter().any(|p| p.ends_with("src/main.rs")),
            "normal source should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("README.md")),
            "top-level markdown should be indexed: {indexed:?}"
        );
        assert!(
            indexed.iter().all(|p| !p.contains("/.bbox/")),
            ".bbox control dir must be excluded from project_file indexing: {indexed:?}"
        );
    }

    #[test]
    fn html_and_xhtml_files_are_admitted_and_claimed_by_html_chunker_not_code_chunker() {
        use std::fs;
        // `code::language_for_path` maps .html/.htm to the "html" tree-sitter
        // grammar, so `CodeChunker::claims` also matches those extensions
        // (verified live: `ts_language_for_name("html")` resolves via
        // tree-sitter-language-pack). `HtmlChunker` MUST be registered
        // before `CodeChunker` in `chunker::default_registry()` for the
        // registry's first-match `find()` (see `index_project` /
        // `resolve_current_chunk_entity` above) to route .html/.htm/.xhtml
        // through prose sectioning rather than code-symbol extraction. This
        // guards that ordering at the registry-integration level, not just
        // inside bbox-chunker's own unit tests.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(
            root.join("page.html"),
            "<html><body><h1>Title</h1><p>hello</p></body></html>",
        )
        .unwrap();
        fs::write(root.join("frag.xhtml"), "<p>fragment</p>").unwrap();

        let mut out = Vec::new();
        scan_project_files(root, &mut out).unwrap();
        let indexed: Vec<String> = out.iter().map(|(p, _, _)| p.clone()).collect();
        assert!(
            indexed.iter().any(|p| p.ends_with("page.html")),
            ".html must be admitted by the project-file walker: {indexed:?}"
        );
        assert!(
            indexed.iter().any(|p| p.ends_with("frag.xhtml")),
            ".xhtml must be admitted by the project-file walker: {indexed:?}"
        );

        let registry = chunker::default_registry();
        let bytes = fs::read(root.join("page.html")).unwrap();
        let sniff_len = bytes.len().min(4096);
        let claimed = registry
            .iter()
            .find(|c| c.claims(Path::new("page.html"), &bytes[..sniff_len]))
            .expect("some chunker must claim page.html");
        assert_eq!(
            claimed.format_id(),
            "html",
            "HtmlChunker must win the registry claim over CodeChunker for .html"
        );

        let (chunks, _edges) = claimed.chunk(Path::new("page.html"), &bytes).unwrap();
        assert!(
            chunks.iter().all(|chunk| chunk.chunk_kind == "web_section"),
            "expected web_section chunks, got {chunks:?}"
        );
    }
}

/// Phase 3 P3-E materialization-migration classification.
///
/// The version bump that ships with this milestone changes every active
/// collected selector's `m` suffix and every collected snapshot id by
/// construction. These rows pin the discriminator between "outgoing, migrate
/// it" and "genuinely inconsistent, fail closed".
#[cfg(test)]
mod collected_materialization_tests {
    use super::*;

    const PROJECT: &str = "p_0000000000000000000000000000ab12";
    const GENERATION: &str = "gen-0123456789abcdef";

    fn activation(
        project_id: &str,
        generation_id: &str,
        selector: &str,
        snapshot_id: &str,
    ) -> bbox_code_source_store::ActivationRecord {
        bbox_code_source_store::ActivationRecord {
            version: 1,
            project_id: project_id.to_string(),
            generation_id: generation_id.to_string(),
            selector: selector.to_string(),
            snapshot_id: snapshot_id.to_string(),
            document_count: 0,
            entity_inventory_sha256: "0".repeat(64),
            current_chunk_targets: Default::default(),
            activated_unix_secs: 0,
            cutback_pending: false,
            diagnostic: None,
        }
    }

    /// A synthetic OUTGOING selector: the same project and generation, a
    /// 16-hex `m` suffix that is not this binary's. `validate_collected_
    /// materialization_selector` is shape-only, so any historic suffix is
    /// well-formed - which is exactly what makes the migration decidable.
    fn outgoing_selector() -> String {
        format!(
            "{}:m{}",
            bbox_code_source::source_selector(PROJECT, GENERATION),
            "0123456789abcdef"
        )
    }

    fn outgoing_snapshot() -> String {
        format!("collected-{}", "9".repeat(32))
    }

    #[test]
    fn a_current_selector_and_snapshot_need_no_migration() {
        let selector = collected_materialization_selector(PROJECT, GENERATION);
        let snapshot = bbox_edge_sidecar::snapshot::collected_snapshot_id(PROJECT, GENERATION);
        let active = ActiveCollectedSource {
            selector: selector.clone(),
            generation_id: GENERATION.into(),
        };
        assert_eq!(
            classify_collected_materialization(
                PROJECT,
                &active,
                &activation(PROJECT, GENERATION, &selector, &snapshot),
            )
            .unwrap(),
            CollectedMaterializationState::Current
        );
    }

    #[test]
    fn an_outgoing_suffix_with_an_agreeing_activation_is_migratable() {
        let selector = outgoing_selector();
        let active = ActiveCollectedSource {
            selector: selector.clone(),
            generation_id: GENERATION.into(),
        };
        assert_eq!(
            classify_collected_materialization(
                PROJECT,
                &active,
                &activation(PROJECT, GENERATION, &selector, &outgoing_snapshot()),
            )
            .unwrap(),
            CollectedMaterializationState::Outgoing
        );
    }

    /// A selector naming a DIFFERENT generation is not an outgoing version, it
    /// is a disagreement about which generation is active.
    #[test]
    fn a_different_generation_still_fails_closed() {
        let selector = format!(
            "{}:m{}",
            bbox_code_source::source_selector(PROJECT, "gen-other"),
            "0123456789abcdef"
        );
        let active = ActiveCollectedSource {
            selector: selector.clone(),
            generation_id: GENERATION.into(),
        };
        let error = classify_collected_materialization(
            PROJECT,
            &active,
            &activation(PROJECT, GENERATION, &selector, &outgoing_snapshot()),
        )
        .err()
        .expect("a foreign generation must fail closed");
        assert!(
            format!("{error:#}").contains("requires materialization migration"),
            "{error:#}"
        );
    }

    #[test]
    fn a_different_project_still_fails_closed() {
        let selector = format!(
            "{}:m{}",
            bbox_code_source::source_selector("p_0000000000000000000000000000ffff", GENERATION),
            "0123456789abcdef"
        );
        let active = ActiveCollectedSource {
            selector: selector.clone(),
            generation_id: GENERATION.into(),
        };
        assert!(
            classify_collected_materialization(
                PROJECT,
                &active,
                &activation(PROJECT, GENERATION, &selector, &outgoing_snapshot()),
            )
            .is_err()
        );
    }

    #[test]
    fn a_malformed_selector_still_fails_closed() {
        for selector in [
            "not-a-selector",
            &format!(
                "{}:mzzzz",
                bbox_code_source::source_selector(PROJECT, GENERATION)
            ),
            &bbox_code_source::source_selector(PROJECT, GENERATION),
        ] {
            let active = ActiveCollectedSource {
                selector: selector.to_string(),
                generation_id: GENERATION.into(),
            };
            assert!(
                classify_collected_materialization(
                    PROJECT,
                    &active,
                    &activation(PROJECT, GENERATION, selector, &outgoing_snapshot()),
                )
                .is_err(),
                "{selector} must fail closed"
            );
        }
    }

    #[test]
    fn a_malformed_snapshot_id_still_fails_closed() {
        let selector = outgoing_selector();
        let active = ActiveCollectedSource {
            selector: selector.clone(),
            generation_id: GENERATION.into(),
        };
        for snapshot in ["head-repo-0123456789ab", "collected-zzzz", "collected-"] {
            assert!(
                classify_collected_materialization(
                    PROJECT,
                    &active,
                    &activation(PROJECT, GENERATION, &selector, snapshot),
                )
                .is_err(),
                "{snapshot} must fail closed"
            );
        }
    }

    /// Selector at the current suffix but a stale snapshot id (or the reverse)
    /// cannot be an outgoing version: both derive from the same version string,
    /// so a split is corruption and keeps the pre-P3-E refusal.
    #[test]
    fn a_split_selector_and_snapshot_still_fails_closed() {
        let current_selector = collected_materialization_selector(PROJECT, GENERATION);
        let current_snapshot =
            bbox_edge_sidecar::snapshot::collected_snapshot_id(PROJECT, GENERATION);
        let active = ActiveCollectedSource {
            selector: current_selector.clone(),
            generation_id: GENERATION.into(),
        };
        let error = classify_collected_materialization(
            PROJECT,
            &active,
            &activation(PROJECT, GENERATION, &current_selector, &outgoing_snapshot()),
        )
        .err()
        .expect("a current selector with a stale snapshot must fail closed");
        assert!(
            format!("{error:#}").contains("requires an explicit migration"),
            "{error:#}"
        );

        let active = ActiveCollectedSource {
            selector: outgoing_selector(),
            generation_id: GENERATION.into(),
        };
        assert!(
            classify_collected_materialization(
                PROJECT,
                &active,
                &activation(PROJECT, GENERATION, &outgoing_selector(), &current_snapshot),
            )
            .is_err(),
            "an outgoing selector with a current snapshot must fail closed"
        );
    }
}

/// Phase 3 P3-C purge and preservation gate (plan section 7 item 2, F2).
///
/// The classification these tests pin is the SHARED one: both the reindex
/// pass and the legacy `build_index` loop route through
/// `classify_stale_meta_row`, so a divergence between the two loops is a
/// compile-time impossibility rather than a review discipline.
#[cfg(test)]
mod purge_exemption_tests {
    use super::*;

    fn local_row(project_id: &str) -> FileMetaSource {
        FileMetaSource::LocalProjectFile {
            project_id: project_id.to_string(),
            selector: bbox_code_source::local_selector(project_id),
            relative_path: "src/lib.rs".into(),
            entry_key: format!("entry-{project_id}"),
        }
    }

    fn meta_row(project_id: &str, entry_suffix: &str) -> FileMeta {
        FileMeta {
            mtime: 1,
            size: 1,
            mat_version: Some("v1".into()),
            source: FileMetaSource::LocalProjectFile {
                project_id: project_id.to_string(),
                selector: bbox_code_source::local_selector(project_id),
                relative_path: format!("src/{entry_suffix}.rs"),
                entry_key: format!("entry-{project_id}-{entry_suffix}"),
            },
        }
    }

    #[test]
    fn non_project_rows_always_keep_the_absolute_path_delete_lane() {
        // Transcripts and pre-`LocalProjectFile` legacy rows key by absolute
        // path; Phase 3 does not touch that lane in either loop.
        assert_eq!(
            classify_stale_meta_row(None, &BTreeSet::new(), &BTreeSet::new()),
            StalePurgeAction::DeleteByPath
        );
        assert_eq!(
            classify_stale_meta_row(
                Some(&FileMetaSource::LegacyFilesystem),
                &BTreeSet::from(["p1".to_string()]),
                &BTreeSet::new()
            ),
            StalePurgeAction::DeleteByPath
        );
    }

    #[test]
    fn a_locally_scanned_project_still_purges_by_entry_key() {
        assert_eq!(
            classify_stale_meta_row(Some(&local_row("p1")), &BTreeSet::new(), &BTreeSet::new()),
            StalePurgeAction::DeleteProjectEntry("entry-p1".into())
        );
    }

    #[test]
    fn every_exempt_state_keeps_its_documents() {
        // Detached, unavailable, cutback-pending, warming-without-local, and
        // empty-root-refused all arrive here as "in the exempt set, not
        // collected", which is the single fact the purge needs (F2 H1/H2/H3).
        assert_eq!(
            classify_stale_meta_row(
                Some(&local_row("p1")),
                &BTreeSet::from(["p1".to_string()]),
                &BTreeSet::new()
            ),
            StalePurgeAction::ExemptRetainRow
        );
    }

    #[test]
    fn a_collected_project_keeps_documents_but_drops_its_local_rows() {
        // Pre-Phase-3 behavior for the collected arm, preserved exactly: the
        // local freshness rows carry no preservation obligation once a
        // collected generation serves the project.
        assert_eq!(
            classify_stale_meta_row(
                Some(&local_row("p1")),
                &BTreeSet::from(["p1".to_string()]),
                &BTreeSet::from(["p1".to_string()])
            ),
            StalePurgeAction::ExemptDropRow
        );
    }

    fn write_local_document(
        writer: &mut IndexWriter,
        f: FieldHandles,
        project_id: &str,
        entry_suffix: &str,
    ) {
        let mut document = TantivyDocument::new();
        document.add_text(f.doc_type, "project_file");
        document.add_text(f.project_id, project_id);
        document.add_text(
            f.code_source_selector,
            &bbox_code_source::local_selector(project_id),
        );
        document.add_text(
            f.code_source_entry_key,
            &format!("entry-{project_id}-{entry_suffix}"),
        );
        document.add_text(
            f.entity_id,
            &format!("project_file:{project_id}:{entry_suffix}"),
        );
        writer.add_document(document).unwrap();
    }

    struct PreservationFixture {
        _dir: tempfile::TempDir,
        index: Index,
        fields: FieldHandles,
        config: ReindexConfig,
    }

    fn preservation_fixture(documents: &[(&str, &str)]) -> PreservationFixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // Built through the real opener so the code tokenizers are
        // registered; a hand-rolled `Index::create_in_dir` cannot write a
        // project-file document at all.
        let transcript_index = crate::index::TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let index = transcript_index.index_handle();
        let fields = transcript_index.field_handles();
        let config = transcript_index.reindex_config();
        let mut writer: IndexWriter = index.writer(50_000_000).unwrap();
        for (project_id, entry) in documents {
            write_local_document(&mut writer, fields, project_id, entry);
        }
        writer.commit().unwrap();
        PreservationFixture {
            _dir: dir,
            index,
            fields,
            config,
        }
    }

    #[test]
    fn detached_preservation_returns_the_documents_its_inventory_promises() {
        let fixture = preservation_fixture(&[("p1", "a"), ("p1", "b")]);
        let meta: HashMap<String, FileMeta> = [
            ("/gone/src/a.rs".to_string(), meta_row("p1", "a")),
            ("/gone/src/b.rs".to_string(), meta_row("p1", "b")),
        ]
        .into_iter()
        .collect();
        let preserved = collect_verified_detached_documents(
            &fixture.index,
            &fixture.config,
            fixture.fields,
            &BTreeSet::from(["p1".to_string()]),
            &meta,
        )
        .unwrap();
        assert_eq!(preserved.len(), 2);
    }

    #[test]
    fn detached_preservation_is_a_no_op_when_nothing_is_indexed_or_promised() {
        let fixture = preservation_fixture(&[]);
        let preserved = collect_verified_detached_documents(
            &fixture.index,
            &fixture.config,
            fixture.fields,
            &BTreeSet::from(["p1".to_string()]),
            &HashMap::new(),
        )
        .unwrap();
        assert!(preserved.is_empty());
    }

    #[test]
    fn detached_preservation_mismatch_aborts_before_any_delete() {
        // The freshness inventory promises two files; only one document is
        // live. The arm must refuse and record `preservation_failed` while
        // the caller is still upstream of `delete_all_documents()`.
        let fixture = preservation_fixture(&[("p1", "a")]);
        let meta: HashMap<String, FileMeta> = [
            ("/gone/src/a.rs".to_string(), meta_row("p1", "a")),
            ("/gone/src/b.rs".to_string(), meta_row("p1", "b")),
        ]
        .into_iter()
        .collect();
        let error = collect_verified_detached_documents(
            &fixture.index,
            &fixture.config,
            fixture.fields,
            &BTreeSet::from(["p1".to_string()]),
            &meta,
        )
        .expect_err("a mismatched inventory must refuse");
        assert!(
            error
                .to_string()
                .contains("preservation inventory mismatch"),
            "{error}"
        );
        let store = bbox_code_source_store::CodeSourceStore::open(
            &fixture.config.code_source_store_path,
            bbox_code_source_store::StoreLimits::default(),
        )
        .unwrap();
        assert!(
            store
                .health_records()
                .unwrap()
                .iter()
                .any(|row| row.project_id == "p1" && row.code == "preservation_failed")
        );
        // The index is untouched: the refusal happened before any deletion.
        let searcher = fixture.index.reader().unwrap().searcher();
        let query = TermQuery::new(
            Term::from_field_text(
                fixture.fields.code_source_selector,
                &bbox_code_source::local_selector("p1"),
            ),
            IndexRecordOption::Basic,
        );
        assert_eq!(searcher.search(&query, &Count).unwrap(), 1);
    }

    #[test]
    fn detached_preservation_refuses_documents_with_no_inventory_at_all() {
        // The inverse direction: documents exist but the freshness rows were
        // lost, so there is no authority to verify them against. Refuse
        // rather than preserve unverified, exactly like the collected arm.
        let fixture = preservation_fixture(&[("p1", "a")]);
        let error = collect_verified_detached_documents(
            &fixture.index,
            &fixture.config,
            fixture.fields,
            &BTreeSet::from(["p1".to_string()]),
            &HashMap::new(),
        )
        .expect_err("documents with no inventory must refuse");
        assert!(
            error
                .to_string()
                .contains("preservation inventory mismatch")
        );
    }

    #[test]
    fn empty_root_probe_matches_the_scan_admission_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let config = preservation_fixture(&[]).config;
        std::fs::create_dir_all(root.join("nested")).unwrap();
        assert!(
            !project_root_has_indexable_entry(&root, &config),
            "a directory tree with no files is empty"
        );
        std::fs::write(root.join("nested/lib.rs"), "fn main() {}\n").unwrap();
        assert!(project_root_has_indexable_entry(&root, &config));
    }
}

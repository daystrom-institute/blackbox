#[cfg(test)]
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::edge_sidecar::Edge;
use crate::manifest::{
    ManifestIndex, OverlayManifest, WorkspaceIndexEntry, WorkspaceManifest, materialized_dir,
};
use bbox_chunker::EdgeProvenance;
use bbox_corpus_core::entity_ref::EntityRef;
use bbox_corpus_core::git_overlay::GitOverlaySelector;

// Paired with `INDEX_SCHEMA_VERSION` in ONE commit at Phase 3 milestone P3-E:
// the schema cut adds `relative_path`/`source_uri`/`source_kind` and stops
// storing absolute values, and this bump invalidates every `FileMeta`
// freshness row and mints new snapshot ids so no document survives under the
// old materialization. Bumping either alone is a defect (the schema drop
// would leave stale per-file freshness rows claiming current materialization,
// or the new snapshot ids would be written into a schema that cannot hold the
// new fields).
const INDEXER_VERSION: &str = "project-index-v2-path-free";
const CHUNKER_VERSION: &str = "chunker-v1";
const DIRTY_OVERLAY_DIRNAME: &str = "dirty-current";
const PENDING_LOCAL_ACTIVATIONS_FILENAME: &str = "pending-local-activations.json";
static MANIFEST_COORDINATOR: OnceLock<Mutex<()>> = OnceLock::new();

fn lock_manifest_coordinator() -> Result<MutexGuard<'static, ()>> {
    MANIFEST_COORDINATOR
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| anyhow::anyhow!("manifest coordinator lock poisoned"))
}

pub fn with_manifest_coordinator<T>(operation: impl FnOnce() -> Result<T>) -> Result<T> {
    let _coordinator = lock_manifest_coordinator()?;
    operation()
}

#[cfg(unix)]
pub fn remove_inactive_materialization_file(
    edges_dir: &Path,
    candidate: &Path,
    expected_identity: (u64, u64),
) -> Result<bool> {
    let relative = candidate
        .strip_prefix(edges_dir)
        .context("inactive materialization candidate escaped its root")?;
    remove_gc_candidate_file(edges_dir, relative, expected_identity, true)
}

#[cfg(unix)]
pub fn remove_gc_candidate_file(
    edges_dir: &Path,
    root_relative: &Path,
    expected_identity: (u64, u64),
    require_inactive: bool,
) -> Result<bool> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    with_manifest_coordinator(|| {
        let candidate = edges_dir.join(root_relative);
        if require_inactive {
            let index = crate::manifest::ManifestIndex::load_or_new(edges_dir)?;
            if index
                .active_paths_for_loader(edges_dir)?
                .iter()
                .any(|active| active.path == candidate)
            {
                anyhow::bail!(
                    "refusing to delete materialization that became active: {}",
                    candidate.display()
                );
            }
        }
        let components = root_relative
            .components()
            .map(|component| match component {
                std::path::Component::Normal(value) => Ok(value.to_os_string()),
                _ => anyhow::bail!("inactive materialization path is not normalized"),
            })
            .collect::<Result<Vec<_>>>()?;
        let Some((leaf, parents)) = components.split_last() else {
            anyhow::bail!("inactive materialization candidate has no leaf");
        };
        let mut directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(edges_dir)?;
        for parent in parents {
            let parent = std::ffi::CString::new(parent.as_bytes())?;
            let fd = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    parent.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            directory = unsafe { fs::File::from_raw_fd(fd) };
        }
        let leaf = std::ffi::CString::new(leaf.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(error.into());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let metadata = file.metadata()?;
        if !metadata.is_file() || (metadata.dev(), metadata.ino()) != expected_identity {
            anyhow::bail!("inactive materialization candidate identity changed before deletion");
        }
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        if unsafe {
            libc::fstatat(
                directory.as_raw_fd(),
                leaf.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        let stat = unsafe { stat.assume_init() };
        if (stat.st_dev as u64, stat.st_ino as u64) != expected_identity {
            anyhow::bail!("inactive materialization candidate was replaced before deletion");
        }
        if unsafe { libc::unlinkat(directory.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory.sync_all()?;
        Ok(true)
    })
}

#[cfg(unix)]
pub(crate) fn write_materialized_file_atomic(
    edges_dir: &Path,
    relative: &Path,
    bytes: &[u8],
) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => Ok(value.to_os_string()),
            _ => anyhow::bail!("materialized write path is not normalized"),
        })
        .collect::<Result<Vec<_>>>()?;
    let Some((leaf, parents)) = components.split_last() else {
        anyhow::bail!("materialized write path has no leaf");
    };
    let root = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(edges_dir)?;
    let mut directory = root;
    for component in
        std::iter::once(std::ffi::OsString::from("materialized")).chain(parents.iter().cloned())
    {
        let component = std::ffi::CString::new(component.as_bytes())?;
        if unsafe { libc::mkdirat(directory.as_raw_fd(), component.as_ptr(), 0o755) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = std::ffi::OsString::from(format!(
        ".{}.{}.{}.tmp",
        leaf.to_string_lossy(),
        std::process::id(),
        sequence
    ));
    let temp_c = std::ffi::CString::new(temp.as_bytes())?;
    let leaf_c = std::ffi::CString::new(leaf.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            temp_c.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
        unsafe { libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(file);
    if unsafe {
        libc::renameat(
            directory.as_raw_fd(),
            temp_c.as_ptr(),
            directory.as_raw_fd(),
            leaf_c.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(directory.as_raw_fd(), temp_c.as_ptr(), 0) };
        return Err(error.into());
    }
    directory.sync_all()?;
    Ok(())
}

/// Combined version stamp that gates per-file re-chunk in the project indexer.
/// `clean_snapshot_id` folds INDEXER_VERSION/CHUNKER_VERSION, so a bump produces
/// a new snapshot id; if the mtime/size skip let unchanged files ride, the new
/// snapshot would be materialized from edges derived under the *old* version.
/// Storing this per file and re-chunking on mismatch closes that gap. The entity
/// parser version is included because it changes the derived edge entity refs.
pub fn current_materialization_version() -> String {
    format!(
        "{}+{}+{}",
        INDEXER_VERSION,
        CHUNKER_VERSION,
        bbox_corpus_core::entity_ref::PARSER_VERSION
    )
}

pub fn clean_snapshot_id(repo_id: &str, project_id: &str, head_sha: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(repo_id.as_bytes());
    hasher.update(project_id.as_bytes());
    hasher.update(head_sha.as_bytes());
    hasher.update(INDEXER_VERSION.as_bytes());
    hasher.update(CHUNKER_VERSION.as_bytes());
    let hash = hasher.finalize();
    let sha_prefix = &head_sha[..head_sha.len().min(12)];
    format!("head-{}-{}", sha_prefix, hex::encode(&hash[..8]))
}

pub fn nongit_snapshot_id(project_id: &str, source_tree_fingerprint: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(project_id.as_bytes());
    hasher.update(source_tree_fingerprint.as_bytes());
    hasher.update(INDEXER_VERSION.as_bytes());
    hasher.update(CHUNKER_VERSION.as_bytes());
    let hash = hasher.finalize();
    format!("nongit-{}", hex::encode(&hash[..16]))
}

pub fn collected_snapshot_id(project_id: &str, generation_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-collected-snapshot-v1");
    hasher.update(project_id.as_bytes());
    hasher.update(generation_id.as_bytes());
    hasher.update(current_materialization_version().as_bytes());
    format!("collected-{}", hex::encode(&hasher.finalize()[..16]))
}

/// Local code-snapshot id for a `LegacyLocal` project whose history record
/// selects [`LegacyLocalSnapshotDerivation::LegacyLocal`] (durable-project-catalog
/// governing section 10.1; Phase 3 plan section 4.6). Unlike
/// `clean_snapshot_id`, this identity is never head-bound: a `LegacyLocal`
/// project's random or absent commit namespace carries no cross-host repo
/// identity to bind against, so the manifest digest alone is the source of
/// truth. `manifest_digest` is the caller-computed hash over the sorted
/// normalized relative path, content hash, and supported-file metadata for
/// the complete local generation (the same complete manifest purge and full
/// rebuild already converge on).
pub fn legacy_local_snapshot_id(project_id: &str, manifest_digest: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"bbox-legacy-local-snapshot-v1");
    hasher.update(project_id.as_bytes());
    hasher.update(manifest_digest.as_bytes());
    hasher.update(current_materialization_version().as_bytes());
    // 32 lowercase hex (16 bytes), matching the existing non-head-bound
    // snapshot-id shape: nongit_snapshot_id and collected_snapshot_id both
    // slice `[..16]`. This id is definitionally not part of the head-bound
    // family (clean_snapshot_id's 16-hex suffix), so it takes the other
    // sibling functions' width, not that one.
    format!("legacylocal-{}", hex::encode(&hasher.finalize()[..16]))
}

/// Which local code-snapshot derivation a project's resolved history record
/// selects (Phase 3 plan section 4.6). Computed from catalog record shape
/// alone, never from a creation-lane notion the catalog does not store, so
/// the same project always re-derives the same answer regardless of who
/// asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LegacyLocalSnapshotDerivation {
    /// No repo history record at all (a non-Git `LegacyLocal` project), or a
    /// history record whose authority is `LocalProject` (a v2-created
    /// attached Git `LegacyLocal` project under its independent random
    /// namespace): use [`legacy_local_snapshot_id`].
    LegacyLocal,
    /// A history record whose authority is `Recorded` or `LegacyNamespace`
    /// (a migrated Git `LegacyLocal` project carrying an imported legacy
    /// namespace): keep the existing head-bound `clean_snapshot_id`
    /// derivation under that namespace, preserving its established ref
    /// shape and commit joins.
    HeadBound,
}

/// Select the derivation for a `LegacyLocal` project from its resolved
/// history record. Bridge local staging is unaffected by this helper: it
/// keeps head-bound clean snapshots unconditionally and never calls it.
pub fn legacy_local_snapshot_derivation(
    repo_history: Option<&bbox_corpus_core::project_catalog::RepoHistoryRecord>,
) -> LegacyLocalSnapshotDerivation {
    use bbox_corpus_core::project_catalog::RepoHistoryAuthority;

    match repo_history {
        None => LegacyLocalSnapshotDerivation::LegacyLocal,
        Some(record) => match &record.authority {
            RepoHistoryAuthority::LocalProject(_) => LegacyLocalSnapshotDerivation::LegacyLocal,
            RepoHistoryAuthority::Recorded(_) | RepoHistoryAuthority::LegacyNamespace(_) => {
                LegacyLocalSnapshotDerivation::HeadBound
            }
        },
    }
}

#[allow(clippy::too_many_arguments)]
pub fn activate_collected_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    generation_id: &str,
    selector: &str,
    snapshot_id: &str,
) -> Result<()> {
    activate_collected_snapshot_with(
        edges_dir,
        project_id,
        repo_id,
        head_sha,
        generation_id,
        selector,
        snapshot_id,
        || Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn activate_collected_snapshot_with<T>(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    generation_id: &str,
    selector: &str,
    snapshot_id: &str,
    after_activation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_manifest_coordinator(|| {
        activate_source_snapshot(
            edges_dir,
            project_id,
            repo_id,
            head_sha,
            generation_id,
            selector,
            snapshot_id,
            false,
            None,
        )?;
        after_activation()
    })
}

#[allow(clippy::too_many_arguments)]
pub fn activate_local_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    selector: &str,
    snapshot_id: &str,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
) -> Result<()> {
    activate_local_snapshot_with(
        edges_dir,
        project_id,
        repo_id,
        head_sha,
        selector,
        snapshot_id,
        dirty,
        dirty_fingerprint,
        || Ok(()),
    )
}

#[allow(clippy::too_many_arguments)]
pub fn activate_local_snapshot_with<T>(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    selector: &str,
    snapshot_id: &str,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
    after_activation: impl FnOnce() -> Result<T>,
) -> Result<T> {
    with_manifest_coordinator(|| {
        activate_source_snapshot(
            edges_dir,
            project_id,
            repo_id,
            head_sha,
            "local",
            selector,
            snapshot_id,
            dirty,
            dirty_fingerprint,
        )?;
        after_activation()
    })
}

#[allow(clippy::too_many_arguments)]
fn activate_source_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    head_sha: &str,
    generation_id: &str,
    selector: &str,
    snapshot_id: &str,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
) -> Result<()> {
    if !snapshot_dir(edges_dir, project_id, snapshot_id).is_dir() {
        anyhow::bail!("code-source edge snapshot is not staged");
    }
    let manifest = WorkspaceManifest {
        version: 1,
        project_id: project_id.to_string(),
        repo_id: Some(repo_id.to_string()),
        canonical_path: None,
        git_common_dir: None,
        git_worktree_dir: None,
        branch: None,
        head_sha: Some(head_sha.to_string()),
        dirty,
        dirty_fingerprint: dirty_fingerprint.map(str::to_string),
        active_snapshot_id: Some(snapshot_id.to_string()),
        active_dirty_overlay_id: None,
        updated_at: None,
    };
    clear_snapshot_staging_marker(edges_dir, project_id, snapshot_id)?;
    WorkspaceManifest::write_to(edges_dir, &manifest)?;

    let mut index = ManifestIndex::load_or_new(edges_dir)?;
    let repo_materialization = index
        .workspaces
        .get(project_id)
        .and_then(|entry| entry.repo_materialization.clone());
    index.upsert_workspace(
        project_id,
        WorkspaceIndexEntry {
            manifest: format!("workspace/{project_id}/manifest.json"),
            active_snapshot: Some(active_snapshot_rel(project_id, snapshot_id)),
            dirty_overlay: None,
            repo_materialization,
            code_source_selector: Some(selector.to_string()),
            code_source_generation: Some(generation_id.to_string()),
            // Activating a new code generation ATOMICALLY CLEARS the
            // project's Git overlay (governing section 11; plan section 10
            // item 1). This assignment is that clear: it happens in the same
            // manifest write as the selector swap, inside the manifest
            // coordinator the caller already holds, so no reader can observe
            // the new generation beside the old generation's overlay. A
            // matching attachment re-establishes the overlay afterwards
            // through `select_git_overlay`; without one the project simply
            // has no overlay, which is the designed steady state rather than
            // a failure.
            git_overlay: None,
            git_overlay_managed: true,
        },
    );
    index.write_atomic(edges_dir)
}

/// Install a Git current-file overlay for an already-active code generation,
/// or clear it (`selector: None`).
///
/// Refuses an overlay whose `code_generation` is not the entry's live one:
/// the whole point of the field is that `COMMIT_TOUCHED_FILE` targets embed a
/// snapshot id, so installing a mismatched overlay would publish dangling
/// edges. A refusal here means the code generation moved while the overlay
/// was being built, and the correct response is to rebuild the overlay, not
/// to relax the check.
///
/// Runs inside the manifest coordinator so the swap cannot interleave with an
/// activation. CALLER CONSTRAINT: never invoke this while holding a staged
/// index generation on the same thread — the writer actor's staging hold and
/// this lock would deadlock (Phase 3 plan carry-forward flag (d)).
pub fn select_git_overlay(
    edges_dir: &Path,
    project_id: &str,
    selector: Option<GitOverlaySelector>,
) -> Result<()> {
    with_manifest_coordinator(|| {
        let mut index = ManifestIndex::load_or_new(edges_dir)?;
        let Some(entry) = index.workspaces.get(project_id).cloned() else {
            anyhow::bail!("workspace manifest entry does not exist for the overlay swap");
        };
        if let Some(selector) = selector.as_ref() {
            if !entry.git_overlay_managed {
                anyhow::bail!(
                    "workspace entry does not own an overlay-managed Git member; \
                     the local reindex lane stages its own"
                );
            }
            let live = entry.code_source_generation.as_deref().unwrap_or_default();
            if !selector.matches_code_generation(live) {
                anyhow::bail!(
                    "Git overlay targets code generation {} but {} is active",
                    selector.code_generation,
                    live
                );
            }
        }
        index.upsert_workspace(
            project_id,
            WorkspaceIndexEntry {
                git_overlay: selector,
                ..entry
            },
        );
        index.write_atomic(edges_dir)
    })
}

/// Every project's currently selected Git overlay, keyed by project id.
///
/// The single read a `CodeReadView` publisher uses to pin `git_overlays`. It
/// reads the manifest rather than any in-memory cache because the manifest is
/// the durable authority for the live selector (plan section 4.7), and
/// because a pinned view built off a stale cache is exactly the incoherence
/// the epoch pin exists to prevent.
pub fn selected_git_overlays(
    edges_dir: &Path,
) -> Result<std::collections::BTreeMap<String, GitOverlaySelector>> {
    let index = ManifestIndex::load_or_new(edges_dir)?;
    Ok(index
        .workspaces
        .iter()
        .filter_map(|(project_id, entry)| {
            entry
                .git_overlay
                .clone()
                .map(|overlay| (project_id.clone(), overlay))
        })
        .collect())
}

pub fn snapshot_dir(edges_dir: &Path, project_id: &str, snapshot_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("snapshots")
        .join(snapshot_id)
}

pub fn dirty_overlay_dir(edges_dir: &Path, project_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join(DIRTY_OVERLAY_DIRNAME)
}

/// ManifestIndex-relative path (under `materialized/`) for a project's active
/// clean snapshot. Single source of truth for the rel-path the manifest stores
/// so callers comparing against the manifest don't re-spell the layout.
pub fn active_snapshot_rel(project_id: &str, snapshot_id: &str) -> String {
    format!("workspace/{}/snapshots/{}", project_id, snapshot_id)
}

/// ManifestIndex-relative path (under `materialized/`) for a project's dirty
/// overlay. Mirrors the value written by `switch_to_dirty_overlay`.
pub fn dirty_overlay_rel(project_id: &str) -> String {
    format!("workspace/{}/{}", project_id, DIRTY_OVERLAY_DIRNAME)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLocalSnapshotActivation {
    project_id: String,
    repo_id: String,
    branch: Option<String>,
    head_sha: String,
    dirty: bool,
    dirty_fingerprint: Option<String>,
    snapshot_id: String,
}

impl PendingLocalSnapshotActivation {
    pub fn project_id(&self) -> &str {
        &self.project_id
    }

    pub fn snapshot_id(&self) -> &str {
        &self.snapshot_id
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingLocalActivationJournal {
    version: u32,
    commit_token: String,
    activations: Vec<PendingLocalSnapshotActivation>,
}

impl PendingLocalActivationJournal {
    pub fn commit_token(&self) -> &str {
        &self.commit_token
    }

    pub fn activations(&self) -> &[PendingLocalSnapshotActivation] {
        &self.activations
    }
}

fn pending_local_activations_path(edges_dir: &Path) -> PathBuf {
    crate::manifest::materialized_dir(edges_dir).join(PENDING_LOCAL_ACTIVATIONS_FILENAME)
}

pub fn write_pending_local_activation_journal(
    edges_dir: &Path,
    activations: &[PendingLocalSnapshotActivation],
) -> Result<PendingLocalActivationJournal> {
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut token = Sha256::new();
    token.update(b"bbox-local-activation-commit-v1");
    token.update(std::process::id().to_be_bytes());
    token.update(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
            .to_be_bytes(),
    );
    for activation in activations {
        token.update(activation.project_id.as_bytes());
        token.update(activation.snapshot_id.as_bytes());
    }
    let journal = PendingLocalActivationJournal {
        version: 1,
        commit_token: hex::encode(token.finalize()),
        activations: activations.to_vec(),
    };
    let path = pending_local_activations_path(edges_dir);
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("pending activation journal has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let mut file = fs::File::create(&temporary)?;
    serde_json::to_writer_pretty(&mut file, &journal)?;
    file.sync_all()?;
    drop(file);
    fs::rename(&temporary, &path)?;
    fs::File::open(parent)?.sync_all()?;
    Ok(journal)
}

pub fn load_pending_local_activation_journal(
    edges_dir: &Path,
) -> Result<Option<PendingLocalActivationJournal>> {
    let path = pending_local_activations_path(edges_dir);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let journal: PendingLocalActivationJournal = serde_json::from_slice(&bytes)?;
    if journal.version != 1 || journal.activations.is_empty() {
        anyhow::bail!("pending local activation journal is invalid");
    }
    Ok(Some(journal))
}

pub fn clear_pending_local_activation_journal(edges_dir: &Path) -> Result<()> {
    let path = pending_local_activations_path(edges_dir);
    match fs::remove_file(&path) {
        Ok(()) => {
            if let Some(parent) = path.parent() {
                fs::File::open(parent)?.sync_all()?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub fn activate_pending_local_snapshots(
    edges_dir: &Path,
    activations: &[PendingLocalSnapshotActivation],
) -> Result<()> {
    with_manifest_coordinator(|| {
        for activation in activations {
            if !snapshot_dir(edges_dir, &activation.project_id, &activation.snapshot_id).is_dir() {
                anyhow::bail!("pending local edge snapshot is not staged");
            }
            let manifest = WorkspaceManifest {
                version: 1,
                project_id: activation.project_id.clone(),
                repo_id: Some(activation.repo_id.clone()),
                canonical_path: None,
                git_common_dir: None,
                git_worktree_dir: None,
                branch: activation.branch.clone(),
                head_sha: Some(activation.head_sha.clone()),
                dirty: activation.dirty,
                dirty_fingerprint: activation.dirty_fingerprint.clone(),
                active_snapshot_id: Some(activation.snapshot_id.clone()),
                active_dirty_overlay_id: None,
                updated_at: None,
            };
            clear_snapshot_staging_marker(
                edges_dir,
                &activation.project_id,
                &activation.snapshot_id,
            )?;
            WorkspaceManifest::write_to(edges_dir, &manifest)?;
        }

        let mut index = ManifestIndex::load_or_new(edges_dir)?;
        for activation in activations {
            // Preserve an existing collected: entry: a project whose
            // effective source is collected must not be overwritten by a
            // local reindex pass. The reindex scans local checkouts and
            // stages local snapshots, but the manifest entry reflects the
            // activation record's authoritative selector. Overwriting a
            // collected entry with local breaks the relationship chain on
            // restart.
            if let Some(existing) = index.workspaces.get(&activation.project_id) {
                if existing
                    .code_source_selector
                    .as_deref()
                    .is_some_and(|s| s.starts_with("collected:"))
                {
                    continue;
                }
            }
            index.upsert_workspace(
                &activation.project_id,
                WorkspaceIndexEntry {
                    manifest: format!("workspace/{}/manifest.json", activation.project_id),
                    active_snapshot: Some(active_snapshot_rel(
                        &activation.project_id,
                        &activation.snapshot_id,
                    )),
                    dirty_overlay: None,
                    repo_materialization: None,
                    code_source_selector: Some(bbox_code_source::local_selector(
                        &activation.project_id,
                    )),
                    code_source_generation: Some("local".to_string()),
                    // The bridge/local reindex lane stages its Git
                    // current-file member inside this same transaction
                    // (plan section 6 item 3 leaves it unchanged), so its
                    // member is never overlay-owned and must keep loading
                    // unconditionally. Setting this true would delete that
                    // lane's commit-file edges at the next rebuild.
                    git_overlay: None,
                    git_overlay_managed: false,
                },
            );
        }
        index.write_atomic(edges_dir)
    })
}

#[allow(clippy::too_many_arguments)]
pub fn stage_local_snapshot_activation(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: &str,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
    snapshot_id: &str,
    project_edges: &[Edge],
    symbol_edges: &[Edge],
    git_current_edges: &[Edge],
) -> Result<PendingLocalSnapshotActivation> {
    write_snapshot_files(
        edges_dir,
        project_id,
        snapshot_id,
        &[
            ("project.jsonl", project_edges),
            ("symbols.jsonl", symbol_edges),
            ("git-current.jsonl", git_current_edges),
        ],
    )?;
    Ok(PendingLocalSnapshotActivation {
        project_id: project_id.to_string(),
        repo_id: repo_id.to_string(),
        branch: branch.map(str::to_string),
        head_sha: head_sha.to_string(),
        dirty,
        dirty_fingerprint: dirty_fingerprint.map(str::to_string),
        snapshot_id: snapshot_id.to_string(),
    })
}

pub fn write_snapshot_files(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    files: &[(&str, &[Edge])],
) -> Result<()> {
    #[cfg(unix)]
    {
        return with_manifest_coordinator(|| {
            validate_snapshot_component(project_id)?;
            validate_snapshot_component(snapshot_id)?;
            write_materialized_file_atomic(
                edges_dir,
                Path::new("workspace")
                    .join(project_id)
                    .join("snapshots")
                    .join(snapshot_id)
                    .join(".staging")
                    .as_path(),
                b"pending\n",
            )?;
            for (filename, edges) in files {
                validate_snapshot_component(filename)?;
                let mut bytes = Vec::new();
                for edge in *edges {
                    serde_json::to_writer(&mut bytes, edge)?;
                    bytes.push(b'\n');
                }
                write_materialized_file_atomic(
                    edges_dir,
                    Path::new("workspace")
                        .join(project_id)
                        .join("snapshots")
                        .join(snapshot_id)
                        .join(filename)
                        .as_path(),
                    &bytes,
                )?;
            }
            Ok(())
        });
    }
    #[cfg(not(unix))]
    {
        let snap_dir = snapshot_dir(edges_dir, project_id, snapshot_id);
        if snap_dir.is_dir() {
            for (filename, edges) in files {
                write_edges_file_atomic(&snap_dir, filename, edges)?;
            }
            fs::File::open(&snap_dir)?.sync_all()?;
            return Ok(());
        }
        let tmp_dir = snap_dir.with_extension("write-tmp");

        if tmp_dir.is_dir() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }
        fs::create_dir_all(&tmp_dir)?;
        for (filename, edges) in files {
            write_edges_file(&tmp_dir.join(*filename), edges)?;
        }
        fs::File::open(&tmp_dir)?.sync_all()?;
        fs::rename(&tmp_dir, &snap_dir)?;
        fs::File::open(
            snap_dir
                .parent()
                .ok_or_else(|| anyhow::anyhow!("snapshot directory has no parent"))?,
        )?
        .sync_all()?;
        Ok(())
    }
}

fn validate_snapshot_component(value: &str) -> Result<()> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path.components().count() != 1
        || !matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        anyhow::bail!("snapshot writer component is not a single normalized name");
    }
    Ok(())
}

fn clear_snapshot_staging_marker(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    let marker = snapshot_dir(edges_dir, project_id, snapshot_id).join(".staging");
    match fs::symlink_metadata(&marker) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            fs::remove_file(&marker)?;
            fs::File::open(
                marker
                    .parent()
                    .ok_or_else(|| anyhow::anyhow!("snapshot staging marker has no parent"))?,
            )?
            .sync_all()?;
            Ok(())
        }
        Ok(_) => anyhow::bail!("snapshot staging marker is not a regular nofollow file"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub struct SnapshotEdgeWriter {
    writer: Option<std::io::BufWriter<fs::File>>,
    temporary: PathBuf,
    destination: PathBuf,
}

impl SnapshotEdgeWriter {
    pub fn append(&mut self, edges: &[Edge]) -> Result<()> {
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("snapshot edge writer is already finished"))?;
        for edge in edges {
            serde_json::to_writer(&mut *writer, edge)?;
            writer.write_all(b"\n")?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<()> {
        let writer = self
            .writer
            .take()
            .ok_or_else(|| anyhow::anyhow!("snapshot edge writer is already finished"))?;
        let file = writer.into_inner().map_err(|error| error.into_error())?;
        file.sync_all()?;
        fs::rename(&self.temporary, &self.destination)?;
        if let Some(parent) = self.destination.parent() {
            fs::File::open(parent)?.sync_all()?;
        }
        Ok(())
    }
}

impl Drop for SnapshotEdgeWriter {
    fn drop(&mut self) {
        if self.writer.is_some() {
            let _ = fs::remove_file(&self.temporary);
        }
    }
}

pub fn create_snapshot_edge_writer(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    filename: &str,
) -> Result<SnapshotEdgeWriter> {
    let directory = snapshot_dir(edges_dir, project_id, snapshot_id);
    fs::create_dir_all(&directory)?;
    let destination = directory.join(filename);
    let temporary = destination.with_extension("jsonl.tmp");
    let file = fs::File::create(&temporary)?;
    Ok(SnapshotEdgeWriter {
        writer: Some(std::io::BufWriter::new(file)),
        temporary,
        destination,
    })
}

#[cfg(not(unix))]
fn write_edges_file_atomic(directory: &Path, filename: &str, edges: &[Edge]) -> Result<()> {
    let path = directory.join(filename);
    let temporary = path.with_extension("jsonl.tmp");
    write_edges_file(&temporary, edges)?;
    fs::rename(&temporary, &path)?;
    Ok(())
}

#[cfg(not(unix))]
fn write_edges_file(path: &Path, edges: &[Edge]) -> Result<()> {
    let file = fs::File::create(path)?;
    // Buffered: one syscall per ~8KiB instead of one per serialized
    // fragment (unbuffered writes dominated snapshot rewrites;
    // thread-935b467d).
    let mut writer = std::io::BufWriter::new(file);
    for edge in edges {
        serde_json::to_writer(&mut writer, edge)?;
        writer.write_all(b"\n")?;
    }
    let file = writer.into_inner().map_err(|err| err.into_error())?;
    file.sync_all()?;
    Ok(())
}

pub fn write_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    files: &[(&str, &[Edge])],
) -> Result<()> {
    for (filename, edges) in files {
        for e in *edges {
            if e.provenance != EdgeProvenance::Derived {
                anyhow::bail!(
                    "dirty overlay rejected non-Derived edge in {}: kind={} provenance={:?} source={:?}",
                    filename,
                    e.kind,
                    e.provenance,
                    e.source,
                );
            }
        }
    }

    let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
    let tmp_dir = overlay_dir.with_extension("write-tmp");

    if tmp_dir.is_dir() {
        let _ = fs::remove_dir_all(&tmp_dir);
    }

    let all_empty = files.iter().all(|(_, edges)| edges.is_empty());
    if all_empty {
        if overlay_dir.is_dir() {
            for entry in fs::read_dir(&overlay_dir)? {
                let entry = entry?;
                if entry.path().extension().and_then(|e| e.to_str()) == Some("jsonl") {
                    fs::remove_file(entry.path())?;
                }
            }
        }
        return Ok(());
    }

    fs::create_dir_all(&tmp_dir)?;

    // Collect covered rel_path_hashes from all overlay edges so the loader
    // can merge snapshot + overlay at per-file granularity.
    let mut covered_hashes = std::collections::HashSet::new();
    for (_filename, edges) in files {
        for edge in *edges {
            if let EntityRef::ProjectFile { rel_path_hash, .. }
            | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &edge.source
            {
                covered_hashes.insert(rel_path_hash.clone());
            }
            if let EntityRef::ProjectFile { rel_path_hash, .. }
            | EntityRef::ProjectFileV2 { rel_path_hash, .. } = &edge.target
            {
                covered_hashes.insert(rel_path_hash.clone());
            }
        }
    }

    for (filename, edges) in files {
        if edges.is_empty() {
            continue;
        }
        let path = tmp_dir.join(*filename);
        let file = fs::File::create(&path)?;
        // Buffered: one syscall per ~8KiB instead of one per serialized
        // fragment (unbuffered writes dominated snapshot rewrites;
        // thread-935b467d).
        let mut writer = std::io::BufWriter::new(file);
        for edge in *edges {
            serde_json::to_writer(&mut writer, edge)?;
            writer.write_all(b"\n")?;
        }
        let file = writer.into_inner().map_err(|err| err.into_error())?;
        file.sync_all()?;
    }

    // Write overlay_manifest.json so the loader knows which hashes are covered.
    OverlayManifest::write_to(&tmp_dir, &covered_hashes)?;

    if overlay_dir.is_dir() {
        let _ = fs::remove_dir_all(&overlay_dir);
    }
    fs::rename(&tmp_dir, &overlay_dir)?;
    Ok(())
}

pub fn clear_dirty_overlay(edges_dir: &Path, project_id: &str) -> Result<bool> {
    let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
    if !overlay_dir.is_dir() {
        return Ok(false);
    }

    let validation = validate_overlay_provenance(&overlay_dir);
    if let Err(bad_files) = validation {
        quarantine_dirty_overlay(edges_dir, project_id, &bad_files)?;
        tracing::warn!(
            project_id,
            ?bad_files,
            "quarantined dirty overlay containing non-Derived provenance"
        );
        return Ok(false);
    }

    fs::remove_dir_all(&overlay_dir)?;
    Ok(true)
}

fn validate_overlay_provenance(overlay_dir: &Path) -> std::result::Result<(), Vec<PathBuf>> {
    let mut bad = Vec::new();
    if let Ok(entries) = fs::read_dir(overlay_dir) {
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            if let Ok(file) = fs::File::open(&path) {
                let reader = std::io::BufReader::new(file);
                use std::io::BufRead;
                for line in reader.lines().map_while(Result::ok) {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(edge) = serde_json::from_str::<Edge>(trimmed) {
                        if edge.provenance != EdgeProvenance::Derived {
                            bad.push(path.clone());
                            break;
                        }
                    }
                }
            }
        }
    }
    if bad.is_empty() { Ok(()) } else { Err(bad) }
}

fn quarantine_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    bad_files: &[PathBuf],
) -> Result<()> {
    let quarantine_dir = materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("dirty-quarantine");

    fs::create_dir_all(&quarantine_dir)?;
    for src in bad_files {
        let filename = src.file_name().unwrap_or_default();
        let dest = quarantine_dir.join(filename);
        if src.exists() {
            fs::rename(src, &dest)?;
        }
    }
    Ok(())
}

pub fn switch_to_clean_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: &str,
    project_edges: Vec<Edge>,
    symbol_edges: Vec<Edge>,
    git_current_edges: Vec<Edge>,
) -> Result<()> {
    let snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
    let snap_path = snapshot_dir(edges_dir, project_id, &snap_id);

    if !snap_path.is_dir() {
        let mut files: Vec<(&str, &[Edge])> = Vec::new();
        let empty: Vec<Edge> = Vec::new();
        files.push(("project.jsonl", &project_edges));
        files.push((
            "symbols.jsonl",
            if symbol_edges.is_empty() {
                &empty
            } else {
                &symbol_edges
            },
        ));
        files.push((
            "git-current.jsonl",
            if git_current_edges.is_empty() {
                &empty
            } else {
                &git_current_edges
            },
        ));
        write_snapshot_files(edges_dir, project_id, &snap_id, &files)?;
    }

    let had_overlay = clear_dirty_overlay(edges_dir, project_id)?;
    if had_overlay {
        tracing::info!(project_id, "cleared dirty overlay on clean checkout");
    }

    update_manifest_for_snapshot(
        edges_dir,
        project_id,
        repo_id,
        branch,
        Some(head_sha),
        false,
        None,
        &snap_id,
        None,
    )?;

    Ok(())
}

pub fn switch_to_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: &str,
    dirty_fingerprint: &str,
    project_edges: Vec<Edge>,
    symbol_edges: Vec<Edge>,
    git_current_edges: Vec<Edge>,
) -> Result<()> {
    let snap_id = clean_snapshot_id(repo_id, project_id, head_sha);
    let snap_path = snapshot_dir(edges_dir, project_id, &snap_id);

    if !snap_path.is_dir() {
        let empty: Vec<Edge> = Vec::new();
        let files: Vec<(&str, &[Edge])> = vec![("project.jsonl", &empty)];
        write_snapshot_files(edges_dir, project_id, &snap_id, &files)?;
    }

    let overlay_files: Vec<(&str, &[Edge])> = vec![
        ("project.jsonl", &project_edges),
        ("symbols.jsonl", &symbol_edges),
        ("git-current.jsonl", &git_current_edges),
    ];
    write_dirty_overlay(edges_dir, project_id, &overlay_files)?;

    update_manifest_for_snapshot(
        edges_dir,
        project_id,
        repo_id,
        branch,
        Some(head_sha),
        true,
        Some(dirty_fingerprint),
        &snap_id,
        Some(&dirty_overlay_rel(project_id)),
    )?;

    Ok(())
}

fn update_manifest_for_snapshot(
    edges_dir: &Path,
    project_id: &str,
    repo_id: &str,
    branch: Option<&str>,
    head_sha: Option<&str>,
    dirty: bool,
    dirty_fingerprint: Option<&str>,
    snapshot_id: &str,
    dirty_overlay_rel: Option<&str>,
) -> Result<()> {
    let _coordinator = lock_manifest_coordinator()?;
    let manifest = WorkspaceManifest {
        version: 1,
        project_id: project_id.to_string(),
        repo_id: Some(repo_id.to_string()),
        canonical_path: None,
        git_common_dir: None,
        git_worktree_dir: None,
        branch: branch.map(|b| b.to_string()),
        head_sha: head_sha.map(|s| s.to_string()),
        dirty,
        dirty_fingerprint: dirty_fingerprint.map(|f| f.to_string()),
        active_snapshot_id: Some(snapshot_id.to_string()),
        active_dirty_overlay_id: dirty_overlay_rel.map(|r| r.to_string()),
        updated_at: None,
    };
    clear_snapshot_staging_marker(edges_dir, project_id, snapshot_id)?;
    WorkspaceManifest::write_to(edges_dir, &manifest)?;

    let mut idx = ManifestIndex::load_or_new(edges_dir)?;

    let snap_rel = active_snapshot_rel(project_id, snapshot_id);
    idx.upsert_workspace(
        project_id,
        WorkspaceIndexEntry {
            manifest: format!("workspace/{}/manifest.json", project_id),
            active_snapshot: Some(snap_rel),
            dirty_overlay: dirty_overlay_rel.map(|r| r.to_string()),
            repo_materialization: None,
            code_source_selector: Some(bbox_code_source::local_selector(project_id)),
            code_source_generation: Some("local".to_string()),
            git_overlay: None,
            git_overlay_managed: false,
        },
    );
    idx.write_atomic(edges_dir)?;

    Ok(())
}

#[cfg(test)]
fn worktree_identity(project_path: &Path) -> (String, Option<String>) {
    let project_id = bbox_corpus_core::entity_ref::project_id_for_path(project_path)
        .unwrap_or_else(|_| hash_path_fallback(project_path));
    let repo_id = discover_repo_id(project_path);
    (project_id, repo_id)
}

#[cfg(test)]
fn hash_path_fallback(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(path.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

#[cfg(test)]
fn discover_repo_id(project_path: &Path) -> Option<String> {
    let git_dir = project_path.join(".git");
    if !git_dir.exists() {
        return None;
    }
    bbox_corpus_core::entity_ref::repo_id_for_path(project_path).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::entity_ref::EntityRef;

    fn make_edge(id: &str, kind: &str, target: &str, prov: EdgeProvenance) -> Edge {
        Edge {
            source: EntityRef::Knowledge { id: id.into() },
            kind: kind.into(),
            target: EntityRef::Knowledge { id: target.into() },
            provenance: prov,
            confidence: bbox_chunker::EdgeConfidence::Exact,
            metadata: BTreeMap::new(),
        }
    }

    fn derived_edge(id: &str, kind: &str, target: &str) -> Edge {
        make_edge(id, kind, target, EdgeProvenance::Derived)
    }

    fn explicit_edge(id: &str, kind: &str, target: &str) -> Edge {
        make_edge(id, kind, target, EdgeProvenance::Explicit)
    }

    #[test]
    fn clean_snapshot_id_differs_across_branches() {
        let a = clean_snapshot_id("repo1", "proj1", "aaa111222333");
        let b = clean_snapshot_id("repo1", "proj1", "bbb444555666");
        assert_ne!(
            a, b,
            "different head_sha must produce different snapshot ids"
        );
    }

    // -- P3-F: Git overlay swap/clear matrix ------------------------------
    //
    // Every case runs through the real manifest coordinator, because the
    // atomicity claim ("activating a new code generation clears the overlay")
    // is a claim about ONE manifest write, not about two writes that usually
    // happen together.

    fn overlay_fixture(edges_dir: &Path, project_id: &str, generation: &str) -> String {
        let snapshot_id = collected_snapshot_id(project_id, generation);
        write_snapshot_files(
            edges_dir,
            project_id,
            &snapshot_id,
            &[
                ("project.jsonl", &[]),
                (crate::manifest::GIT_CURRENT_MEMBER, &[]),
            ],
        )
        .unwrap();
        activate_collected_snapshot(
            edges_dir,
            project_id,
            "repo-authority",
            &"a".repeat(40),
            generation,
            &format!("collected:{project_id}:{generation}"),
            &snapshot_id,
        )
        .unwrap();
        snapshot_id
    }

    fn overlay_for(project_id: &str, generation: &str) -> GitOverlaySelector {
        GitOverlaySelector {
            project_id: project_id.to_string(),
            code_generation: generation.to_string(),
            repo_history_generation: format!("rhg_{}", "a".repeat(64)),
            attachment_id: "att_1".to_string(),
            repo_head: "b".repeat(40),
            commit_namespace: "nsmono".to_string(),
            overlay_generation: 1,
        }
    }

    fn git_current_is_loaded(edges_dir: &Path) -> bool {
        ManifestIndex::load_or_new(edges_dir)
            .unwrap()
            .active_paths_for_loader(edges_dir)
            .unwrap()
            .iter()
            .any(|loadable| {
                loadable.path.file_name().and_then(|name| name.to_str())
                    == Some(crate::manifest::GIT_CURRENT_MEMBER)
            })
    }

    #[test]
    fn activation_leaves_no_overlay_and_gates_the_git_member_off() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        overlay_fixture(&edges_dir, "p_1", "gen-a");

        let index = ManifestIndex::load_or_new(&edges_dir).unwrap();
        let entry = index.workspaces.get("p_1").unwrap();
        assert!(entry.git_overlay.is_none());
        assert!(entry.git_overlay_managed);
        assert!(
            !git_current_is_loaded(&edges_dir),
            "a freshly activated generation has no overlay, so its stale Git \
             member must not be loaded"
        );
    }

    #[test]
    fn selecting_an_overlay_admits_the_git_member_and_clearing_removes_it() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        overlay_fixture(&edges_dir, "p_1", "gen-a");

        select_git_overlay(&edges_dir, "p_1", Some(overlay_for("p_1", "gen-a"))).unwrap();
        assert!(git_current_is_loaded(&edges_dir));
        assert_eq!(
            selected_git_overlays(&edges_dir).unwrap().len(),
            1,
            "the read-view publisher must see the selection"
        );

        select_git_overlay(&edges_dir, "p_1", None).unwrap();
        assert!(!git_current_is_loaded(&edges_dir));
        assert!(selected_git_overlays(&edges_dir).unwrap().is_empty());
    }

    #[test]
    fn activating_a_new_generation_atomically_clears_the_overlay() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        overlay_fixture(&edges_dir, "p_1", "gen-a");
        select_git_overlay(&edges_dir, "p_1", Some(overlay_for("p_1", "gen-a"))).unwrap();
        assert!(git_current_is_loaded(&edges_dir));

        // Activating gen-b without a usable attachment: the overlay clears in
        // the same manifest write that swaps the selector, so no reader can
        // observe gen-b beside gen-a's overlay.
        overlay_fixture(&edges_dir, "p_1", "gen-b");
        let index = ManifestIndex::load_or_new(&edges_dir).unwrap();
        let entry = index.workspaces.get("p_1").unwrap();
        assert!(entry.git_overlay.is_none());
        assert_eq!(entry.code_source_generation.as_deref(), Some("gen-b"));
        assert!(
            !git_current_is_loaded(&edges_dir),
            "gen-a's COMMIT_TOUCHED_FILE targets name a retired snapshot id"
        );
    }

    #[test]
    fn an_overlay_for_a_foreign_code_generation_is_refused() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        overlay_fixture(&edges_dir, "p_1", "gen-b");
        let error = select_git_overlay(&edges_dir, "p_1", Some(overlay_for("p_1", "gen-a")))
            .unwrap_err()
            .to_string();
        assert!(error.contains("gen-a"), "{error}");
        assert!(!git_current_is_loaded(&edges_dir));
    }

    #[test]
    fn a_local_reindex_entry_keeps_its_git_member_without_an_overlay() {
        // Bridge parity: the local reindex lane stages its Git member inside
        // its own transaction and never writes a selector. Gating that
        // member on a selector it does not write would silently delete its
        // commit-file edges.
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = "head-local";
        let pending = stage_local_snapshot_activation(
            &edges_dir,
            "p_local",
            "repo-authority",
            None,
            &"c".repeat(40),
            false,
            None,
            snapshot_id,
            &[],
            &[],
            &[],
        )
        .unwrap();
        activate_pending_local_snapshots(&edges_dir, &[pending]).unwrap();

        let index = ManifestIndex::load_or_new(&edges_dir).unwrap();
        let entry = index.workspaces.get("p_local").unwrap();
        assert!(!entry.git_overlay_managed);
        assert!(entry.git_overlay.is_none());
        assert!(
            git_current_is_loaded(&edges_dir),
            "the local lane's own Git member must keep loading unconditionally"
        );
        assert!(
            select_git_overlay(&edges_dir, "p_local", Some(overlay_for("p_local", "local")))
                .is_err(),
            "a non-overlay-managed entry must refuse a selector rather than \
             hand its member's lifecycle to a lane that does not own it"
        );
    }

    #[test]
    fn clean_snapshot_id_is_deterministic() {
        let id1 = clean_snapshot_id("repo1", "proj1", "abc123");
        let id2 = clean_snapshot_id("repo1", "proj1", "abc123");
        assert_eq!(id1, id2);
    }

    #[test]
    fn clean_snapshot_id_excludes_dirty_fingerprint() {
        let id = clean_snapshot_id("repo1", "proj1", "abc123def456");
        assert!(
            id.starts_with("head-abc123def456-"),
            "snapshot id should start with head sha prefix, got: {id}"
        );
    }

    #[test]
    fn nongit_snapshot_id_differs_by_fingerprint() {
        let a = nongit_snapshot_id("proj1", "fp-a");
        let b = nongit_snapshot_id("proj1", "fp-b");
        assert_ne!(a, b);
    }

    #[test]
    fn legacy_local_snapshot_id_is_deterministic_and_never_head_bound() {
        let a = legacy_local_snapshot_id("proj1", "digest-a");
        let b = legacy_local_snapshot_id("proj1", "digest-a");
        assert_eq!(a, b);
        assert!(a.starts_with("legacylocal-"));
        assert!(!a.contains("head-"));
        // 32 lowercase hex, matching nongit_snapshot_id/collected_snapshot_id's
        // width, not clean_snapshot_id's 16-hex head-bound suffix.
        let hex = a.strip_prefix("legacylocal-").unwrap();
        assert_eq!(hex.len(), 32);
        assert!(
            hex.bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        );
    }

    #[test]
    fn legacy_local_snapshot_id_differs_by_project_and_by_digest() {
        let a = legacy_local_snapshot_id("proj1", "digest-a");
        let b = legacy_local_snapshot_id("proj1", "digest-b");
        let c = legacy_local_snapshot_id("proj2", "digest-a");
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn legacy_local_snapshot_derivation_selects_by_record_shape() {
        use bbox_corpus_core::project_catalog::{
            CommitNamespace, RecordedRepoAuthority, RepoHistoryAuthority, RepoHistoryId,
            RepoHistoryMaterialization, RepoHistoryRecord,
        };

        assert_eq!(
            legacy_local_snapshot_derivation(None),
            LegacyLocalSnapshotDerivation::LegacyLocal,
            "a project with no history record at all (non-Git LegacyLocal) uses legacylocal"
        );

        let local = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::mint(),
            authority: RepoHistoryAuthority::LocalProject(
                bbox_corpus_core::project_catalog::ProjectId::parse("p_local").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("local_abc").unwrap(),
            compatibility_namespaces: Default::default(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        assert_eq!(
            legacy_local_snapshot_derivation(Some(&local)),
            LegacyLocalSnapshotDerivation::LegacyLocal,
            "LocalProject authority under its own random namespace uses legacylocal"
        );

        let imported = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::mint(),
            authority: RepoHistoryAuthority::LegacyNamespace(
                CommitNamespace::parse("legacy-namespace").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("legacy-namespace").unwrap(),
            compatibility_namespaces: Default::default(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        assert_eq!(
            legacy_local_snapshot_derivation(Some(&imported)),
            LegacyLocalSnapshotDerivation::HeadBound,
            "an imported legacy namespace keeps the head-bound derivation"
        );

        let recorded = RepoHistoryRecord {
            repo_history_id: RepoHistoryId::mint(),
            authority: RepoHistoryAuthority::Recorded(
                RecordedRepoAuthority::parse("repo-a").unwrap(),
            ),
            primary_namespace: CommitNamespace::parse("repo-a").unwrap(),
            compatibility_namespaces: Default::default(),
            materialization: RepoHistoryMaterialization::NotBuilt,
        };
        assert_eq!(
            legacy_local_snapshot_derivation(Some(&recorded)),
            LegacyLocalSnapshotDerivation::HeadBound,
            "Recorded authority never applies to a real LegacyLocal project, but the helper \
             still resolves it as head-bound rather than fabricating a legacylocal answer"
        );
    }

    #[test]
    fn write_snapshot_creates_files_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_snapshot_files(edges_dir, "p1", "snap-001", &[("project.jsonl", &edges)]).unwrap();

        let snap = snapshot_dir(edges_dir, "p1", "snap-001");
        assert!(snap.is_dir(), "snapshot dir must exist");
        let project_jsonl = snap.join("project.jsonl");
        assert!(project_jsonl.exists(), "project.jsonl must exist");

        let content = fs::read_to_string(&project_jsonl).unwrap();
        assert!(
            content.contains("k1"),
            "project.jsonl must contain edge data"
        );
    }

    #[test]
    fn write_snapshot_atomic_on_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_dir = snapshot_dir(edges_dir, "p1", "snap-conflict");
        fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("project.jsonl"), "stale").unwrap();

        let edges = vec![derived_edge("k_new", "DESCRIBES", "k_target")];
        write_snapshot_files(
            edges_dir,
            "p1",
            "snap-conflict",
            &[("project.jsonl", &edges)],
        )
        .unwrap();

        let content = fs::read_to_string(snap_dir.join("project.jsonl")).unwrap();
        assert!(
            content.contains("k_new"),
            "overwritten snapshot must have new edge"
        );
        assert!(
            !content.contains("stale"),
            "overwritten snapshot must not have old content"
        );
    }

    #[test]
    fn dirty_overlay_writes_derived_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k_dirty", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.is_dir());
        let content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(content.contains("k_dirty"));
    }

    #[test]
    fn dirty_overlay_rejects_explicit_edges() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![explicit_edge("k_exp", "DESCRIBES", "k_target")];
        let result = write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]);
        assert!(result.is_err(), "must reject explicit edges");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("non-Derived"),
            "error must mention non-Derived: {err}"
        );
    }

    #[test]
    fn dirty_overlay_replaces_previous() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_v1 = vec![derived_edge("k_v1", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges_v1)]).unwrap();

        let edges_v2 = vec![derived_edge("k_v2", "DESCRIBES", "k_target")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges_v2)]).unwrap();

        let content =
            fs::read_to_string(dirty_overlay_dir(edges_dir, "p1").join("project.jsonl")).unwrap();
        assert!(content.contains("k_v2"), "overlay must contain new edge");
        assert!(
            !content.contains("k_v1"),
            "overlay must not contain old edge"
        );
    }

    #[test]
    fn clear_dirty_overlay_removes_dir() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();

        let cleared = clear_dirty_overlay(edges_dir, "p1").unwrap();
        assert!(cleared, "should report overlay was cleared");
        assert!(
            !dirty_overlay_dir(edges_dir, "p1").exists(),
            "overlay dir must be gone"
        );
    }

    #[test]
    fn clear_dirty_overlay_returns_false_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let cleared = clear_dirty_overlay(dir.path(), "p1").unwrap();
        assert!(!cleared, "should report nothing to clear");
    }

    #[test]
    fn clear_dirty_overlay_quarantines_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        fs::create_dir_all(&overlay).unwrap();
        let explicit_edge_line =
            serde_json::to_string(&explicit_edge("k_exp", "DESCRIBES", "k2")).unwrap();
        fs::write(overlay.join("project.jsonl"), explicit_edge_line).unwrap();

        let cleared = clear_dirty_overlay(edges_dir, "p1").unwrap();
        assert!(
            !cleared,
            "should not report success for quarantined overlay"
        );

        let quarantine = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("dirty-quarantine")
            .join("project.jsonl");
        assert!(quarantine.exists(), "quarantined file must exist");
    }

    #[test]
    fn switch_to_clean_creates_snapshot_and_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            edges.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_id = clean_snapshot_id("repo1", "p1", "abc123def456");
        let snap = snapshot_dir(edges_dir, "p1", &snap_id);
        assert!(snap.is_dir(), "snapshot dir must exist");

        let manifest_path = crate::manifest::WorkspaceManifest::manifest_path(edges_dir, "p1");
        let manifest = WorkspaceManifest::read_from(&manifest_path).unwrap();
        assert_eq!(
            manifest.active_snapshot_id.as_deref(),
            Some(snap_id.as_str())
        );
        assert!(!manifest.dirty);
        assert!(manifest.active_dirty_overlay_id.is_none());
    }

    #[test]
    fn switch_to_clean_reuses_existing_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_branch_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa1111",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let edges_b = vec![derived_edge("k_branch_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb2222",
            edges_b.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa1111",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a = clean_snapshot_id("repo1", "p1", "sha_aaaa1111");
        let snap_dir = snapshot_dir(edges_dir, "p1", &snap_a);
        let content = fs::read_to_string(snap_dir.join("project.jsonl")).unwrap();
        assert!(
            content.contains("k_branch_a"),
            "reused snapshot must have original edge"
        );
    }

    #[test]
    fn switch_to_dirty_creates_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            "fp-dirty-v1",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.is_dir(), "dirty overlay dir must exist");
        let content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(content.contains("k_dirty"));

        let manifest_path = WorkspaceManifest::manifest_path(edges_dir, "p1");
        let manifest = WorkspaceManifest::read_from(&manifest_path).unwrap();
        assert!(manifest.dirty);
        assert_eq!(manifest.dirty_fingerprint.as_deref(), Some("fp-dirty-v1"));
        assert!(manifest.active_dirty_overlay_id.is_some());
    }

    #[test]
    fn clean_checkout_clears_dirty_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("feature"),
            "sha111222333",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();
        assert!(dirty_overlay_dir(edges_dir, "p1").is_dir());

        let clean_edges = vec![derived_edge("k_clean", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha444555666",
            clean_edges,
            vec![],
            vec![],
        )
        .unwrap();

        assert!(
            !dirty_overlay_dir(edges_dir, "p1").exists(),
            "dirty overlay must be cleared on clean checkout"
        );

        let manifest =
            WorkspaceManifest::read_from(&WorkspaceManifest::manifest_path(edges_dir, "p1"))
                .unwrap();
        assert!(!manifest.dirty);
        assert!(manifest.active_dirty_overlay_id.is_none());
    }

    #[test]
    fn inactive_snapshot_does_not_affect_manifest_index() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let edges_b = vec![derived_edge("k_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
        let snap_a_dir = snapshot_dir(edges_dir, "p1", &snap_a_id);
        assert!(
            snap_a_dir.is_dir(),
            "inactive snapshot must still exist on disk"
        );

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let entry = &idx.workspaces["p1"];
        let active_snap = entry.active_snapshot.as_ref().unwrap();
        assert!(
            active_snap.contains("sha_bbbb"),
            "active snapshot must be branch-b, got: {active_snap}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn confined_snapshot_and_manifest_writers_refuse_symlinked_parents() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        fs::create_dir_all(edges_dir.join("materialized")).unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(
            outside.path(),
            edges_dir.join("materialized").join("workspace"),
        )
        .unwrap();
        let edges = Vec::<Edge>::new();
        assert!(
            write_snapshot_files(&edges_dir, "p1", "snap1", &[("project.jsonl", &edges)]).is_err()
        );
        let manifest = WorkspaceManifest {
            version: 1,
            project_id: "p1".into(),
            repo_id: None,
            canonical_path: None,
            git_common_dir: None,
            git_worktree_dir: None,
            branch: None,
            head_sha: None,
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("snap1".into()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        assert!(WorkspaceManifest::write_to(&edges_dir, &manifest).is_err());
        assert!(fs::read_dir(outside.path()).unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn inactive_gc_revalidates_activation_before_unlink() {
        use std::os::unix::fs::MetadataExt;

        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let edges = Vec::<Edge>::new();
        write_snapshot_files(&edges_dir, "p1", "snap1", &[("project.jsonl", &edges)]).unwrap();
        let candidate = snapshot_dir(&edges_dir, "p1", "snap1").join("project.jsonl");
        let metadata = fs::symlink_metadata(&candidate).unwrap();
        let mut index = ManifestIndex::new();
        index.upsert_workspace(
            "p1",
            WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/snap1".into()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some("local:p1".into()),
                code_source_generation: Some("local".into()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(&edges_dir).unwrap();
        assert!(
            remove_inactive_materialization_file(
                &edges_dir,
                &candidate,
                (metadata.dev(), metadata.ino())
            )
            .is_err()
        );
        assert!(candidate.is_file());
    }

    #[test]
    fn switching_back_reuses_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a.clone(),
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_content_before = {
            let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
            let snap_a = snapshot_dir(edges_dir, "p1", &snap_a_id);
            fs::read_to_string(snap_a.join("project.jsonl")).unwrap()
        };

        let edges_b = vec![derived_edge("k_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let snap_a_id = clean_snapshot_id("repo1", "p1", "sha_aaaa");
        let snap_a_content_after = {
            let snap_a = snapshot_dir(edges_dir, "p1", &snap_a_id);
            fs::read_to_string(snap_a.join("project.jsonl")).unwrap()
        };
        assert_eq!(
            snap_a_content_before, snap_a_content_after,
            "switching back must reuse identical snapshot content"
        );
    }

    #[test]
    fn worktree_identity_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let (project_id, repo_id) = worktree_identity(dir.path());
        assert!(!project_id.is_empty());
        assert!(repo_id.is_none(), "non-git dir should have no repo_id");
    }

    #[test]
    fn worktree_identity_with_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let (project_id, repo_id) = worktree_identity(dir.path());
        assert!(!project_id.is_empty());
        assert!(repo_id.is_some(), "git dir should yield repo_id");
    }

    #[test]
    fn dirty_overlay_empty_edges_cleans_up() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(edges_dir, "p1", &[("project.jsonl", &edges)]).unwrap();
        assert!(dirty_overlay_dir(edges_dir, "p1").is_dir());

        write_dirty_overlay(edges_dir, "p1", &[]).unwrap();
        let overlay = dirty_overlay_dir(edges_dir, "p1");
        if overlay.is_dir() {
            let has_jsonl = fs::read_dir(&overlay)
                .unwrap()
                .filter_map(Result::ok)
                .any(|e| e.path().extension().and_then(|e| e.to_str()) == Some("jsonl"));
            assert!(!has_jsonl, "empty overlay write must remove jsonl files");
        }
    }

    #[test]
    fn stale_temp_dir_does_not_leak_into_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_dir = snapshot_dir(edges_dir, "p1", "snap-stale");
        let tmp_dir = snap_dir.with_extension("write-tmp");
        fs::create_dir_all(&tmp_dir).unwrap();
        fs::write(tmp_dir.join("stale.jsonl"), "should not persist").unwrap();

        let edges = vec![derived_edge("k_clean", "DESCRIBES", "k2")];
        write_snapshot_files(edges_dir, "p1", "snap-stale", &[("project.jsonl", &edges)]).unwrap();

        assert!(
            !snap_dir.join("stale.jsonl").exists(),
            "stale temp file must not leak into snapshot"
        );
        assert!(
            snap_dir.join("project.jsonl").exists(),
            "new snapshot files must exist"
        );
    }

    #[test]
    fn dirty_overlay_writes_multiple_lanes() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let proj = vec![derived_edge("k_proj", "DESCRIBES", "k2")];
        let sym = vec![derived_edge("k_sym", "HAS_SYMBOL", "k2")];
        let git = vec![derived_edge("k_git", "EDITED_FILE", "k2")];

        write_dirty_overlay(
            edges_dir,
            "p1",
            &[
                ("project.jsonl", &proj),
                ("symbols.jsonl", &sym),
                ("git-current.jsonl", &git),
            ],
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        let proj_content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        let sym_content = fs::read_to_string(overlay.join("symbols.jsonl")).unwrap();
        let git_content = fs::read_to_string(overlay.join("git-current.jsonl")).unwrap();

        assert!(proj_content.contains("k_proj"));
        assert!(sym_content.contains("k_sym"));
        assert!(git_content.contains("k_git"));
    }

    #[test]
    fn switch_to_dirty_writes_all_lanes_to_overlay() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let proj = vec![derived_edge("k_proj", "DESCRIBES", "k2")];
        let sym = vec![derived_edge("k_sym", "HAS_SYMBOL", "k2")];
        let git = vec![derived_edge("k_git", "EDITED_FILE", "k2")];

        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "abc123def456",
            "fp-dirty",
            proj,
            sym,
            git,
        )
        .unwrap();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        assert!(overlay.join("project.jsonl").exists());
        assert!(overlay.join("symbols.jsonl").exists());
        assert!(overlay.join("git-current.jsonl").exists());

        let proj_content = fs::read_to_string(overlay.join("project.jsonl")).unwrap();
        assert!(proj_content.contains("k_proj"));
        let sym_content = fs::read_to_string(overlay.join("symbols.jsonl")).unwrap();
        assert!(sym_content.contains("k_sym"));
    }

    #[test]
    fn active_loader_overlay_suppresses_clean_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            snap_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let paths = idx.active_materialized_paths(edges_dir);

        let has_overlay = paths
            .iter()
            .any(|p| p.to_str().unwrap_or_default().contains("dirty-current"));
        let has_snap = paths.iter().any(|p| {
            p.to_str()
                .unwrap_or_default()
                .contains("snapshots/head-sha_aaaa")
        });
        let snap_edge_in_paths = paths.iter().any(|p| {
            fs::read_to_string(p)
                .unwrap_or_default()
                .contains("k_snap_only")
        });
        assert!(has_overlay, "active loader must include dirty overlay");
        assert!(
            !has_snap,
            "whole-workspace dirty overlay must suppress clean snapshot paths"
        );
        assert!(
            !snap_edge_in_paths,
            "clean-snapshot-only edge must not appear in active paths"
        );
    }

    #[test]
    fn clean_checkout_restores_clean_snapshot_in_active_paths() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let snap_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            snap_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let dirty_edges = vec![derived_edge("k_dirty", "DESCRIBES", "k2")];
        switch_to_dirty_overlay(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            "fp-dirty",
            dirty_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let clean_edges = vec![derived_edge("k_snap_only", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("main"),
            "sha_aaaa",
            clean_edges,
            vec![],
            vec![],
        )
        .unwrap();

        let idx = ManifestIndex::load(edges_dir).unwrap();
        let paths = idx.active_materialized_paths(edges_dir);

        let has_overlay = paths
            .iter()
            .any(|p| p.to_str().unwrap_or_default().contains("dirty-current"));
        let has_snap = paths.iter().any(|p| {
            p.to_str()
                .unwrap_or_default()
                .contains("snapshots/head-sha_aaaa")
        });
        assert!(
            !has_overlay,
            "clean checkout must remove dirty overlay from active paths"
        );
        assert!(
            has_snap,
            "clean checkout must restore clean snapshot in active paths"
        );
    }

    #[test]
    fn branch_switch_changes_active_graph_via_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k_branch_a", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-a"),
            "sha_aaaa",
            edges_a,
            vec![],
            vec![],
        )
        .unwrap();

        let idx_a = ManifestIndex::load(edges_dir).unwrap();
        let paths_a = idx_a.active_materialized_paths(edges_dir);
        let has_a = paths_a.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_a")
        });
        assert!(has_a, "branch-a active graph must have branch-a edges");

        let edges_b = vec![derived_edge("k_branch_b", "DESCRIBES", "k2")];
        switch_to_clean_snapshot(
            edges_dir,
            "p1",
            "repo1",
            Some("branch-b"),
            "sha_bbbb",
            edges_b,
            vec![],
            vec![],
        )
        .unwrap();

        let idx_b = ManifestIndex::load(edges_dir).unwrap();
        let paths_b = idx_b.active_materialized_paths(edges_dir);
        let has_b = paths_b.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_b")
        });
        let has_a_in_b = paths_b.iter().any(|p| {
            let content = fs::read_to_string(p).unwrap_or_default();
            content.contains("k_branch_a")
        });
        assert!(has_b, "branch-b active graph must have branch-b edges");
        assert!(
            !has_a_in_b,
            "branch-b active graph must NOT have branch-a edges"
        );
    }

    #[test]
    fn pending_local_activations_publish_as_one_manifest_index() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();
        let first_snapshot = clean_snapshot_id("repo-1", "project-1", "aaaa");
        let second_snapshot = clean_snapshot_id("repo-2", "project-2", "bbbb");
        let first = stage_local_snapshot_activation(
            edges_dir,
            "project-1",
            "repo-1",
            Some("main"),
            "aaaa",
            false,
            None,
            &first_snapshot,
            &[derived_edge("first", "DESCRIBES", "target")],
            &[],
            &[],
        )
        .unwrap();
        let second = stage_local_snapshot_activation(
            edges_dir,
            "project-2",
            "repo-2",
            Some("main"),
            "bbbb",
            false,
            None,
            &second_snapshot,
            &[derived_edge("second", "DESCRIBES", "target")],
            &[],
            &[],
        )
        .unwrap();

        let journal = write_pending_local_activation_journal(edges_dir, &[first, second]).unwrap();
        assert_eq!(journal.activations().len(), 2);
        assert!(
            load_pending_local_activation_journal(edges_dir)
                .unwrap()
                .is_some()
        );

        activate_pending_local_snapshots(edges_dir, journal.activations()).unwrap();
        let manifest = ManifestIndex::load(edges_dir).unwrap();
        assert_eq!(manifest.workspaces.len(), 2);
        assert_eq!(
            manifest.workspaces["project-1"]
                .code_source_selector
                .as_deref(),
            Some("local:project-1")
        );
        let expected_second = active_snapshot_rel("project-2", &second_snapshot);
        assert_eq!(
            manifest.workspaces["project-2"].active_snapshot.as_deref(),
            Some(expected_second.as_str())
        );

        clear_pending_local_activation_journal(edges_dir).unwrap();
        assert!(
            load_pending_local_activation_journal(edges_dir)
                .unwrap()
                .is_none()
        );
    }

    /// Regression: a background reindex that stages a local snapshot for a
    /// project whose effective source is collected must NOT overwrite the
    /// manifest index entry with a `local:` selector. If it does, the
    /// restart chain validation (`validate_relationship_chain`) catches the
    /// selector mismatch and refuses boot. The fix is in
    /// `activate_pending_local_snapshots`: it preserves any existing
    /// `collected:` entry rather than blindly upserting `local:`.
    #[test]
    fn reindex_preserves_collected_entry_against_local_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let project_id = "neutral-collision-winner";
        let generation = "gen-collected-1";
        let collected_snap = collected_snapshot_id(project_id, generation);
        let collected_sel = format!("collected:{project_id}:{generation}");

        // Step 1: simulate a collected activation (as reconstruction would
        // produce during pre-bind recovery). This writes a collected entry
        // into the manifest index.
        write_snapshot_files(
            edges_dir,
            project_id,
            &collected_snap,
            &[("project.jsonl", &[])],
        )
        .unwrap();

        let mut index = ManifestIndex::new();
        index.upsert_workspace(
            project_id,
            WorkspaceIndexEntry {
                manifest: format!("workspace/{project_id}/manifest.json"),
                active_snapshot: Some(active_snapshot_rel(project_id, &collected_snap)),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: Some(collected_sel.clone()),
                code_source_generation: Some(generation.to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(edges_dir).unwrap();

        // Step 2: simulate the background reindex staging a local snapshot
        // for the same project and calling activate_pending_local_snapshots.
        let local_snap = clean_snapshot_id("repo-1", project_id, "aaaa");
        let activation = stage_local_snapshot_activation(
            edges_dir,
            project_id,
            "repo-1",
            Some("main"),
            "aaaa",
            false,
            None,
            &local_snap,
            &[derived_edge("k_local", "DESCRIBES", "k_target")],
            &[],
            &[],
        )
        .unwrap();

        activate_pending_local_snapshots(edges_dir, &[activation]).unwrap();

        // Step 3: assert the manifest entry still carries the collected
        // selector (not overwritten with local:).
        let manifest = ManifestIndex::load(edges_dir).unwrap();
        let entry = manifest
            .workspaces
            .get(project_id)
            .expect("project entry must exist after reindex");
        assert_eq!(
            entry.code_source_selector.as_deref(),
            Some(collected_sel.as_str()),
            "collected entry must survive a local reindex pass"
        );
        assert_eq!(
            entry.code_source_generation.as_deref(),
            Some(generation),
            "collected generation must survive a local reindex pass"
        );

        // Step 4: simulate restart - load the manifest fresh and verify the
        // entry is still collected (chain validation would pass).
        let reloaded = ManifestIndex::load(edges_dir).unwrap();
        let entry_after = reloaded
            .workspaces
            .get(project_id)
            .expect("project entry must exist after reload");
        assert!(
            entry_after
                .code_source_selector
                .as_deref()
                .is_some_and(|s| s.starts_with("collected:")),
            "persisted entry must carry collected selector across restart"
        );
    }
}

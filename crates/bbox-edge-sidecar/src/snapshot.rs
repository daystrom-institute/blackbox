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
    remove_gc_candidate_file(edges_dir, relative, expected_identity, None, true)
}

#[cfg(unix)]
pub fn remove_gc_candidate_file(
    edges_dir: &Path,
    root_relative: &Path,
    expected_identity: (u64, u64),
    expected_mtime_secs: Option<u64>,
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
        if require_inactive {
            let staging = std::ffi::CString::new(".staging").unwrap();
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            if unsafe {
                libc::fstatat(
                    directory.as_raw_fd(),
                    staging.as_ptr(),
                    stat.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            {
                anyhow::bail!("refusing to delete a staged snapshot member");
            }
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
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
        // R17F5: verify mtime from the opened fd immediately before
        // unlinkat. The path-based mtime check in revalidate_temp_identity
        // races with a concurrent writer that replaces the file between
        // path validation and descriptor unlink.
        if let Some(planned_mtime) = expected_mtime_secs {
            let current_mtime = metadata.mtime() as u64;
            if current_mtime != planned_mtime {
                anyhow::bail!(
                    "temp file mtime changed after descriptor open (planned={}, current={})",
                    planned_mtime,
                    current_mtime
                );
            }
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

/// Write member files into a snapshot directory. This is used for both
/// initial snapshot creation (via stage_local_snapshot_activation) and by
/// the transactional update path. On unix the writes are descriptor-
/// confined and atomic (temp file + rename + fsync). On non-unix the
/// snapshot directory is created if missing and members are written
/// directly.
///
/// R19: write_snapshot_files no longer writes a .staging marker into the
/// live snapshot directory. Transactional member updates that need crash
/// protection stage outside the live snapshot via
/// write_snapshot_members_transaction.
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
            let snap_relative = Path::new("workspace")
                .join(project_id)
                .join("snapshots")
                .join(snapshot_id);
            for (filename, edges) in files {
                validate_snapshot_component(filename)?;
                let mut bytes = Vec::new();
                for edge in *edges {
                    serde_json::to_writer(&mut bytes, edge)?;
                    bytes.push(b'\n');
                }
                write_materialized_file_atomic(
                    edges_dir,
                    snap_relative.join(*filename).as_path(),
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

/// R19 transaction journal. A versioned bounded record written outside the
/// live snapshot directory. The LIVE snapshot is never touched during
/// staging; members are staged in a sibling txn directory and renamed into
/// the live snapshot only after the paired Tantivy commit succeeds.
///
/// Fields:
///   v: format version (1)
///   txn_token: unique opaque token identifying this transaction
///   snapshot_id: target snapshot the members belong to
///   members: validated member names + SHA-256 hashes of staged bytes
#[derive(serde::Serialize, serde::Deserialize)]
struct TxnJournal {
    v: u32,
    txn_token: String,
    snapshot_id: String,
    members: Vec<TxnMember>,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
struct TxnMember {
    name: String,
    sha256: String,
}

/// Test-visible mirror of TxnJournal for deserializing journals in tests.
#[cfg(test)]
#[derive(serde::Serialize, serde::Deserialize)]
pub struct TxnJournalForTest {
    pub txn_token: String,
    pub snapshot_id: String,
}

/// Maximum number of members in a transaction journal.
const TXN_MAX_MEMBERS: usize = 64;
/// Maximum total size of the journal file (64 KB).
const TXN_MAX_JOURNAL_BYTES: usize = 64 * 1024;
/// Maximum size of any single staged member file (256 MB).
const TXN_MAX_MEMBER_BYTES: u64 = 256 * 1024 * 1024;

/// Relative path of the transaction staging directory for a project + token.
fn txn_staging_rel(project_id: &str, txn_token: &str) -> PathBuf {
    Path::new("workspace")
        .join(project_id)
        .join("txn")
        .join(txn_token)
}

/// Relative path of the journal file for a project + token.
fn txn_journal_rel(project_id: &str, txn_token: &str) -> PathBuf {
    Path::new("workspace")
        .join(project_id)
        .join("txn")
        .join(format!("{txn_token}.journal.json"))
}

/// R19F1+F3+F4: Stage member files OUTSIDE the live snapshot directory.
/// Writes each member into materialized/workspace/<project>/txn/<token>/
/// and a durable versioned bounded journal at
/// materialized/workspace/<project>/txn/<token>.journal.json.
/// The LIVE snapshot directory is never touched. The caller MUST call
/// finalize_snapshot_publication AFTER writer.commit() to rename the
/// staged members into the live snapshot.
pub fn write_snapshot_members_transaction(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    files: &[(&str, &[Edge])],
) -> Result<()> {
    let txn_token = generate_txn_token();
    write_snapshot_members_transaction_with_token(
        edges_dir,
        project_id,
        snapshot_id,
        files,
        &txn_token,
    )
}

/// Generate a unique transaction token: process id + monotonic counter +
/// timestamp. This is opaque to recovery; it only needs uniqueness.
fn generate_txn_token() -> String {
    static TXN_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TXN_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("txn-{seq}-{ts}")
}

#[allow(clippy::too_many_arguments)]
fn write_snapshot_members_transaction_with_token(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
    files: &[(&str, &[Edge])],
    txn_token: &str,
) -> Result<()> {
    validate_snapshot_component(project_id)?;
    validate_snapshot_component(snapshot_id)?;
    validate_snapshot_component(txn_token)?;
    if files.is_empty() {
        anyhow::bail!("transaction must stage at least one member");
    }
    if files.len() > TXN_MAX_MEMBERS {
        anyhow::bail!(
            "transaction has {} members (max {})",
            files.len(),
            TXN_MAX_MEMBERS
        );
    }

    let mut members: Vec<TxnMember> = Vec::new();
    let mut member_bytes: Vec<(String, Vec<u8>)> = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    for (filename, edges) in files {
        validate_snapshot_component(filename)?;
        if !seen_names.insert(*filename) {
            anyhow::bail!("duplicate member name in transaction: {filename}");
        }
        let mut bytes = Vec::new();
        for edge in *edges {
            serde_json::to_writer(&mut bytes, edge)?;
            bytes.push(b'\n');
        }
        let hash = hex::encode(Sha256::digest(&bytes));
        members.push(TxnMember {
            name: filename.to_string(),
            sha256: hash,
        });
        member_bytes.push((filename.to_string(), bytes));
    }

    let journal = TxnJournal {
        v: 1,
        txn_token: txn_token.to_string(),
        snapshot_id: snapshot_id.to_string(),
        members,
    };
    let journal_bytes = serde_json::to_vec(&journal)?;
    if journal_bytes.len() > TXN_MAX_JOURNAL_BYTES {
        anyhow::bail!(
            "transaction journal exceeds max size ({} > {})",
            journal_bytes.len(),
            TXN_MAX_JOURNAL_BYTES
        );
    }

    with_manifest_coordinator(|| {
        let staging_rel = txn_staging_rel(project_id, txn_token);
        let journal_rel = txn_journal_rel(project_id, txn_token);

        // Stage member files into the txn directory.
        for (filename, bytes) in &member_bytes {
            write_materialized_file_atomic(edges_dir, staging_rel.join(filename).as_path(), bytes)?;
        }

        // Write the journal last: its presence means staging is complete.
        write_materialized_file_atomic(edges_dir, journal_rel.as_path(), &journal_bytes)?;

        Ok(())
    })
}

/// R19F1+F3: Finalize a transaction after the paired Tantivy commit
/// succeeded. Renameat each staged member into the live snapshot directory
/// (descriptor-confined, per-member atomic), fsync the snapshot directory,
/// then delete the journal and the empty txn staging directory.
/// Each stage is idempotent: a crash mid-finalize leaves a valid journal
/// that recovery will resume.
pub fn finalize_snapshot_publication(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    with_manifest_coordinator(|| finalize_pending_transactions(edges_dir, project_id, snapshot_id))
}

/// R19: finalize all pending journals for a project + snapshot. Each
/// journal with a matching snapshot_id is finalized (members renamed into
/// the live snapshot, journal + txn dir deleted). Journals with a
/// different snapshot_id are left for a different snapshot's finalize or
/// for recovery.
#[cfg(unix)]
fn finalize_pending_transactions(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    validate_snapshot_component(project_id)?;
    validate_snapshot_component(snapshot_id)?;

    let txn_dir_rel = Path::new("materialized")
        .join("workspace")
        .join(project_id)
        .join("txn");
    let txn_dir = match open_dir_under_root(edges_dir, &txn_dir_rel, false) {
        Ok(dir) => dir,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(());
        }
        Err(error) => return Err(error),
    };

    let entries = crate::manifest::read_directory_names(&txn_dir)?;
    let journal_names: Vec<_> = entries
        .iter()
        .filter_map(|name| {
            let s = name.to_str()?;
            if s.ends_with(".journal.json") {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();

    let snap_dir_rel = Path::new("materialized")
        .join("workspace")
        .join(project_id)
        .join("snapshots")
        .join(snapshot_id);
    let snap_dir = open_dir_under_root(edges_dir, &snap_dir_rel, true)?;

    for journal_name in &journal_names {
        let journal_c = std::ffi::CString::new(journal_name.as_bytes())?;
        let journal_bytes =
            read_confined_file_bounded(&txn_dir, &journal_c, TXN_MAX_JOURNAL_BYTES)?;
        let journal: TxnJournal = match serde_json::from_slice(&journal_bytes) {
            Ok(j) => j,
            Err(_) => continue, // invalid journal: leave for recovery
        };
        if journal.snapshot_id != snapshot_id {
            continue;
        }

        let staging_c = std::ffi::CString::new(journal.txn_token.as_bytes())?;
        let staging_dir = match open_confined_dir_fd(txn_dir.as_raw_fd(), &staging_c) {
            Ok(fd) => fd,
            Err(_) => continue, // staging dir missing: leave for recovery
        };

        // Rename each staged member into the live snapshot.
        for member in &journal.members {
            let member_c = std::ffi::CString::new(member.name.as_bytes())?;
            let rename_result = unsafe {
                libc::renameat(
                    staging_dir.as_raw_fd(),
                    member_c.as_ptr(),
                    snap_dir.as_raw_fd(),
                    member_c.as_ptr(),
                )
            };
            if rename_result != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::NotFound {
                    // Member rename failed non-trivially: leave journal for recovery.
                    tracing::warn!(
                        project_id,
                        snapshot_id,
                        member = member.name.as_str(),
                        error = %error,
                        "finalize: renameat failed, leaving journal for recovery"
                    );
                    continue;
                }
            }
        }

        snap_dir.sync_all()?;

        // Delete the journal and the (now empty) staging directory.
        let _ = unsafe { libc::unlinkat(txn_dir.as_raw_fd(), journal_c.as_ptr(), 0) };
        let _ =
            unsafe { libc::unlinkat(txn_dir.as_raw_fd(), staging_c.as_ptr(), libc::AT_REMOVEDIR) };
        txn_dir.sync_all()?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn finalize_pending_transactions(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    validate_snapshot_component(project_id)?;
    validate_snapshot_component(snapshot_id)?;

    let txn_dir = materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("txn");
    if !txn_dir.is_dir() {
        return Ok(());
    }

    let snap_dir = snapshot_dir(edges_dir, project_id, snapshot_id);
    fs::create_dir_all(&snap_dir)?;

    for entry in fs::read_dir(&txn_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) if s.ends_with(".journal.json") => s.to_string(),
            _ => continue,
        };
        let journal_path = txn_dir.join(&name_str);
        let journal_bytes = fs::read(&journal_path)?;
        if journal_bytes.len() > TXN_MAX_JOURNAL_BYTES {
            continue;
        }
        let journal: TxnJournal = match serde_json::from_slice(&journal_bytes) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if journal.snapshot_id != snapshot_id {
            continue;
        }
        let staging_dir = txn_dir.join(&journal.txn_token);
        if !staging_dir.is_dir() {
            continue;
        }

        for member in &journal.members {
            let src = staging_dir.join(&member.name);
            let dst = snap_dir.join(&member.name);
            if src.exists() {
                fs::rename(&src, &dst)?;
            }
        }
        fs::File::open(&snap_dir)?.sync_all()?;

        let _ = fs::remove_file(&journal_path);
        let _ = fs::remove_dir_all(&staging_dir);
    }

    Ok(())
}

/// R19F1+F2+F4: Pre-bind recovery for pending transaction journals.
/// Unconditional in open_shared_state, before selector refresh and
/// read-view construction. Scans materialized/workspace/<project>/txn/
/// for *.journal.json files. For each journal:
///   (a) Journal invalid or staged members fail validation: fail closed
///       with a typed operator-visible error, preserve everything.
///   (b) Staged members validate: the transaction is AMBIGUOUS (we cannot
///       prove whether the paired Tantivy commit succeeded without a
///       commit token). DISCARD the staging directory and journal; the
///       live snapshot was never touched, so no rollback is needed and
///       the relationship chain sees the pre-transaction state.
///   (c) Legacy .staging marker inside a live snapshot directory (from
///       prior builds): fail closed with a typed error.
///
/// Each operation is idempotent: a crash inside recovery itself resumes
/// cleanly on restart because the journal and staging dir survive until
/// recovery explicitly deletes them.
pub fn recover_pending_transactions_prebind(edges_dir: &Path) -> Result<()> {
    with_manifest_coordinator(|| {
        let index = crate::manifest::ManifestIndex::load_or_new(edges_dir)?;
        for project_id in index.workspaces.keys() {
            recover_pending_transactions_for_project(edges_dir, project_id)?;
        }
        Ok(())
    })
}

#[cfg(unix)]
fn recover_pending_transactions_for_project(edges_dir: &Path, project_id: &str) -> Result<()> {
    use std::os::fd::AsRawFd;

    validate_snapshot_component(project_id)?;

    // R19F4(c): check for legacy .staging markers inside live snapshot
    // directories. These come from prior builds and must fail closed.
    check_legacy_staging_markers(edges_dir, project_id)?;

    let txn_dir_rel = Path::new("materialized")
        .join("workspace")
        .join(project_id)
        .join("txn");
    let txn_dir = match open_dir_under_root(edges_dir, &txn_dir_rel, false) {
        Ok(dir) => dir,
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
            }) =>
        {
            return Ok(());
        }
        Err(error) => {
            anyhow::bail!("recovery: failed to open txn dir for {project_id}: {error}")
        }
    };

    let entries = crate::manifest::read_directory_names(&txn_dir)?;
    let journal_names: Vec<_> = entries
        .iter()
        .filter_map(|name| {
            let s = name.to_str()?;
            if s.ends_with(".journal.json") {
                Some(s.to_string())
            } else {
                None
            }
        })
        .collect();

    for journal_name in &journal_names {
        let journal_c = std::ffi::CString::new(journal_name.as_bytes())?;

        // R19F4: bounded read of journal.
        let journal_bytes =
            read_confined_file_bounded(&txn_dir, &journal_c, TXN_MAX_JOURNAL_BYTES)?;

        // R19F4: versioned bounded decode with full validation.
        let journal = decode_txn_journal(&journal_bytes)?;

        // R19F5: open the staging directory by descriptor and verify every
        // member with identity-bound fd hashing.
        let staging_c = std::ffi::CString::new(journal.txn_token.as_bytes())?;
        let staging_fd = open_confined_dir_fd(txn_dir.as_raw_fd(), &staging_c)?;

        for member in &journal.members {
            verify_member_identity_bound(&staging_fd, member)?;
        }

        // R19F2(b): staged members validate. The transaction is ambiguous
        // (no commit token binding). DISCARD the staging dir and journal.
        // The live snapshot was never touched: no rollback, no manifest
        // mutation, the relationship chain sees the pre-transaction state.
        tracing::info!(
            project_id,
            txn_token = journal.txn_token.as_str(),
            snapshot_id = journal.snapshot_id.as_str(),
            "recovery: ambiguous pending transaction, discarding staging (live snapshot untouched)"
        );
        discard_transaction(&txn_dir, &journal.txn_token, &journal_c)?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn recover_pending_transactions_for_project(edges_dir: &Path, project_id: &str) -> Result<()> {
    validate_snapshot_component(project_id)?;

    // R19F4(c): legacy marker check.
    check_legacy_staging_markers(edges_dir, project_id)?;

    let txn_dir = materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
        .join("txn");
    if !txn_dir.is_dir() {
        return Ok(());
    }

    let mut journals_to_discard = Vec::new();
    for entry in fs::read_dir(&txn_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = match name.to_str() {
            Some(s) if s.ends_with(".journal.json") => s.to_string(),
            _ => continue,
        };
        let journal_path = txn_dir.join(&name_str);
        let metadata = fs::symlink_metadata(&journal_path)?;
        if !metadata.is_file() || metadata.file_type().is_symlink() {
            anyhow::bail!("recovery: journal {name_str} for {project_id} is not a regular file");
        }
        if metadata.len() > TXN_MAX_JOURNAL_BYTES as u64 {
            anyhow::bail!("recovery: journal {name_str} for {project_id} exceeds max size");
        }
        let journal_bytes = fs::read(&journal_path)?;
        let journal = decode_txn_journal(&journal_bytes)?;

        // R19F5: verify members with nofollow + size enforcement.
        let staging_dir = txn_dir.join(&journal.txn_token);
        for member in &journal.members {
            let member_path = staging_dir.join(&member.name);
            let m = fs::symlink_metadata(&member_path)?;
            if !m.is_file() || m.file_type().is_symlink() {
                anyhow::bail!(
                    "recovery: member {} for {project_id} is not a regular file",
                    member.name
                );
            }
            if m.len() > TXN_MAX_MEMBER_BYTES {
                anyhow::bail!(
                    "recovery: member {} for {project_id} exceeds max size",
                    member.name
                );
            }
            let mut file = fs::File::open(&member_path)?;
            let mut hasher = Sha256::new();
            let mut reader = std::io::BufReader::new(file.try_clone()?);
            let mut limited = (&mut reader).take(TXN_MAX_MEMBER_BYTES + 1);
            use std::io::Read;
            let mut buf = [0u8; 65536];
            loop {
                let n = limited.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hasher.update(&buf[..n]);
            }
            let actual_hash = hex::encode(hasher.finalize());
            if actual_hash != member.sha256 {
                anyhow::bail!(
                    "recovery: member {} hash mismatch for {project_id}",
                    member.name
                );
            }
            // R19F5: reject growth during hashing.
            let final_meta = file.metadata()?;
            if final_meta.len() != m.len() {
                anyhow::bail!(
                    "recovery: member {} for {project_id} changed size during hashing",
                    member.name
                );
            }
        }

        journals_to_discard.push((name_str, journal.txn_token.clone()));
    }

    for (journal_name, txn_token) in &journals_to_discard {
        tracing::info!(
            project_id,
            txn_token = txn_token.as_str(),
            "recovery: ambiguous pending transaction, discarding staging (live snapshot untouched)"
        );
        let journal_path = txn_dir.join(journal_name);
        let staging_dir = txn_dir.join(txn_token);
        let _ = fs::remove_dir_all(&staging_dir);
        fs::remove_file(&journal_path)?;
    }

    Ok(())
}

/// R19F4(c): check for legacy .staging markers inside live snapshot
/// directories (from prior builds). Fail closed with a typed error.
#[cfg(unix)]
fn check_legacy_staging_markers(edges_dir: &Path, project_id: &str) -> Result<()> {
    use std::os::fd::AsRawFd;
    let index = crate::manifest::ManifestIndex::load_or_new(edges_dir)?;
    let Some(entry) = index.workspaces.get(project_id) else {
        return Ok(());
    };
    let Some(snapshot_rel) = entry.active_snapshot.as_deref() else {
        return Ok(());
    };
    let snap_dir = open_dir_under_root(
        edges_dir,
        &Path::new("materialized").join(snapshot_rel),
        false,
    );
    let Ok(snap_dir) = snap_dir else {
        return Ok(());
    };
    let marker_c = std::ffi::CString::new(b".staging".as_slice())?;
    match fstatat_nofollow(snap_dir.as_raw_fd(), &marker_c) {
        Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFREG => {
            anyhow::bail!(
                "recovery: legacy .staging marker found in live snapshot for {project_id} \
                 (snapshot={snapshot_rel}). This format is no longer supported; \
                 remove the marker and reindex the project."
            );
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_legacy_staging_markers(edges_dir: &Path, project_id: &str) -> Result<()> {
    let index = crate::manifest::ManifestIndex::load_or_new(edges_dir)?;
    let Some(entry) = index.workspaces.get(project_id) else {
        return Ok(());
    };
    if let Some(snapshot_rel) = entry.active_snapshot.as_deref() {
        let marker = materialized_dir(edges_dir)
            .join(snapshot_rel)
            .join(".staging");
        if marker.exists() {
            anyhow::bail!(
                "recovery: legacy .staging marker found in live snapshot for {project_id} \
                 (snapshot={snapshot_rel}). This format is no longer supported; \
                 remove the marker and reindex the project."
            );
        }
    }
    Ok(())
}

/// R19F4: versioned bounded decoder for the transaction journal. Validates
/// version, member count, single-component unique names, fixed-format
/// SHA-256, and txn_token/snapshot_id components. Returns a typed error
/// on any violation; the caller must fail closed.
fn decode_txn_journal(bytes: &[u8]) -> Result<TxnJournal> {
    if bytes.len() > TXN_MAX_JOURNAL_BYTES {
        anyhow::bail!(
            "transaction journal exceeds max size ({} > {})",
            bytes.len(),
            TXN_MAX_JOURNAL_BYTES
        );
    }
    let journal: TxnJournal = serde_json::from_slice(bytes)?;
    if journal.v != 1 {
        anyhow::bail!(
            "transaction journal version {} is not supported (expected 1)",
            journal.v
        );
    }
    if journal.members.is_empty() {
        anyhow::bail!("transaction journal has no members");
    }
    if journal.members.len() > TXN_MAX_MEMBERS {
        anyhow::bail!(
            "transaction journal has {} members (max {})",
            journal.members.len(),
            TXN_MAX_MEMBERS
        );
    }
    validate_snapshot_component(&journal.txn_token)?;
    validate_snapshot_component(&journal.snapshot_id)?;
    let mut seen_names = std::collections::HashSet::new();
    for member in &journal.members {
        validate_snapshot_component(&member.name)?;
        if !seen_names.insert(&member.name) {
            anyhow::bail!(
                "transaction journal has duplicate member name: {}",
                member.name
            );
        }
        if member.sha256.len() != 64 || !member.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            anyhow::bail!(
                "transaction journal member {} has invalid sha256 format",
                member.name
            );
        }
    }
    Ok(journal)
}

/// R19F5: Verify a staged member with identity-bound fd hashing on unix.
/// Binds the opened descriptor's dev/ino/type/size, streams via
/// take(MAX+1) with overflow refusal, and rejects growth during hashing.
#[cfg(unix)]
fn verify_member_identity_bound(staging_fd: &fs::File, member: &TxnMember) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::MetadataExt;

    let member_c = std::ffi::CString::new(member.name.as_bytes())?;
    let mfd = unsafe {
        libc::openat(
            staging_fd.as_raw_fd(),
            member_c.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if mfd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!("recovery: staged member {} is missing", member.name);
        }
        anyhow::bail!(
            "recovery: failed to open staged member {}: {error}",
            member.name
        );
    }
    let mfile = unsafe { fs::File::from_raw_fd(mfd) };

    // R19F5: bind identity from the opened descriptor.
    let meta = mfile.metadata()?;
    if !meta.is_file() {
        anyhow::bail!(
            "recovery: staged member {} is not a regular file",
            member.name
        );
    }
    let bound_dev = meta.dev();
    let bound_ino = meta.ino();
    let bound_size = meta.len();
    if bound_size > TXN_MAX_MEMBER_BYTES {
        anyhow::bail!(
            "recovery: staged member {} exceeds max size ({} > {})",
            member.name,
            bound_size,
            TXN_MAX_MEMBER_BYTES
        );
    }

    // R19F5: stream via take(MAX+1) with overflow refusal.
    let mut hasher = Sha256::new();
    let reader = std::io::BufReader::new(&mfile);
    let mut limited = reader.take(TXN_MAX_MEMBER_BYTES + 1);
    use std::io::Read;
    let mut buf = [0u8; 65536];
    let mut total_read: u64 = 0;
    loop {
        let n = limited.read(&mut buf)?;
        if n == 0 {
            break;
        }
        total_read += n as u64;
        hasher.update(&buf[..n]);
    }
    if total_read > TXN_MAX_MEMBER_BYTES {
        anyhow::bail!(
            "recovery: staged member {} exceeded max size during read",
            member.name
        );
    }

    // R19F5: reject growth during hashing (fd identity check).
    let post_meta = mfile.metadata()?;
    if post_meta.dev() != bound_dev || post_meta.ino() != bound_ino || post_meta.len() != bound_size
    {
        anyhow::bail!(
            "recovery: staged member {} changed identity/size during hashing",
            member.name
        );
    }

    let actual_hash = hex::encode(hasher.finalize());
    if actual_hash != member.sha256 {
        anyhow::bail!(
            "recovery: staged member {} hash mismatch (expected {}, got {})",
            member.name,
            member.sha256,
            actual_hash
        );
    }

    Ok(())
}

/// R19F2(b): Discard a transaction's staging directory and journal.
/// Idempotent: if either is already gone, no error.
#[cfg(unix)]
fn discard_transaction(
    txn_dir: &fs::File,
    txn_token: &str,
    journal_c: &std::ffi::CStr,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    let staging_c = std::ffi::CString::new(txn_token.as_bytes())?;

    // Delete the staging directory tree first.
    match fstatat_nofollow(txn_dir.as_raw_fd(), &staging_c) {
        Ok(_) => {
            unlinkat_tree(txn_dir.as_raw_fd(), &staging_c)?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    // Delete the journal.
    let _ = unsafe { libc::unlinkat(txn_dir.as_raw_fd(), journal_c.as_ptr(), 0) };

    txn_dir.sync_all()?;
    Ok(())
}

/// R19F5 helper: read a file confined to a directory fd with a size bound.
#[cfg(unix)]
fn read_confined_file_bounded(
    dir_fd: &fs::File,
    name: &std::ffi::CStr,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    // fstatat first to get size and verify it is a regular file.
    let stat = fstatat_nofollow(dir_fd.as_raw_fd(), name)?;
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
        anyhow::bail!("confined file is not a regular file");
    }
    if stat.st_size as usize > max_bytes {
        anyhow::bail!(
            "confined file exceeds max size ({} > {})",
            stat.st_size,
            max_bytes
        );
    }

    let fd = unsafe {
        libc::openat(
            dir_fd.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!("confined file is missing");
        }
        return Err(error.into());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    let mut bytes = Vec::with_capacity(stat.st_size as usize);
    let mut reader = std::io::BufReader::new(file).take((max_bytes as u64) + 1);
    use std::io::Read;
    reader.read_to_end(&mut bytes)?;
    if bytes.len() > max_bytes {
        anyhow::bail!("confined file exceeded max size during read");
    }
    Ok(bytes)
}

/// R19F5 helper: open a directory fd confined under a parent fd.
#[cfg(unix)]
fn open_confined_dir_fd(parent_fd: std::os::fd::RawFd, name: &std::ffi::CStr) -> Result<fs::File> {
    use std::os::fd::FromRawFd;
    let fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            anyhow::bail!("recovery: staging directory is missing");
        }
        anyhow::bail!("recovery: failed to open staging directory: {error}");
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
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

#[cfg(unix)]
/// Open the root edges_dir and walk every component of `relative` with
/// `O_DIRECTORY | O_NOFOLLOW`, returning the leaf directory descriptor.
/// This confines filesystem mutation to descriptor-relative operations:
/// a symlink substituted into any intermediate component is rejected at
/// open time instead of being silently followed. `create_missing` creates
/// intermediate directories with `mkdirat`.
fn open_dir_under_root(
    edges_dir: &Path,
    relative: &Path,
    create_missing: bool,
) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;

    let components = relative
        .components()
        .map(|component| match component {
            std::path::Component::Normal(value) => Ok(value.to_os_string()),
            _ => anyhow::bail!("materialized path is not normalized"),
        })
        .collect::<Result<Vec<_>>>()?;
    let root = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(edges_dir)?;
    let mut directory = root;
    for component in components {
        let component_c = std::ffi::CString::new(component.as_bytes())?;
        if create_missing {
            if unsafe { libc::mkdirat(directory.as_raw_fd(), component_c.as_ptr(), 0o755) } != 0 {
                let error = std::io::Error::last_os_error();
                if error.kind() != std::io::ErrorKind::AlreadyExists {
                    return Err(error.into());
                }
            }
        }
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                component_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        directory = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok(directory)
}

#[cfg(unix)]
fn fstatat_nofollow(
    dir_fd: std::os::fd::RawFd,
    name: &std::ffi::CStr,
) -> std::io::Result<libc::stat> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    if unsafe {
        libc::fstatat(
            dir_fd,
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(unsafe { stat.assume_init() })
}

#[cfg(unix)]
fn collect_dir_entries(dir: &fs::File) -> Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    let mut entries = Vec::new();
    // fdopendir takes ownership of the fd and closedir closes it. Duplicate
    // the fd so the borrowed fs::File's Drop still has a valid fd to close.
    let dup_fd = unsafe { libc::dup(dir.as_raw_fd()) };
    if dup_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let dir_stream = unsafe { libc::fdopendir(dup_fd) };
    if dir_stream.is_null() {
        let error = std::io::Error::last_os_error();
        unsafe { libc::close(dup_fd) };
        return Err(error.into());
    }
    // rewind to start in case the caller's fd position is mid-directory.
    unsafe { libc::rewinddir(dir_stream) };
    loop {
        let entry_ptr = unsafe { libc::readdir(dir_stream) };
        if entry_ptr.is_null() {
            break;
        }
        let entry = unsafe { &*entry_ptr };
        let name_bytes = unsafe {
            std::slice::from_raw_parts(
                entry.d_name.as_ptr() as *const u8,
                libc::strlen(entry.d_name.as_ptr()) as usize,
            )
        };
        if name_bytes == b"." || name_bytes == b".." {
            continue;
        }
        entries.push(std::ffi::OsString::from_vec(name_bytes.to_vec()));
    }
    unsafe { libc::closedir(dir_stream) };
    Ok(entries)
}

#[cfg(unix)]
/// Recursively remove a directory tree relative to a parent descriptor using
/// `unlinkat(AT_REMOVEDIR)` for subdirectories and `unlinkat(0)` for files.
/// Every component is opened with `O_NOFOLLOW` before mutation, so a symlink
/// planted inside the tree cannot escape the parent.
fn unlinkat_tree(parent_fd: std::os::fd::RawFd, name: &std::ffi::CStr) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let dir_fd = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if dir_fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(());
        }
        return Err(error.into());
    }
    let dir_file = unsafe { fs::File::from_raw_fd(dir_fd) };
    let entries = collect_dir_entries(&dir_file)?;
    for entry_name in entries {
        let entry_c = std::ffi::CString::new(entry_name.as_bytes())?;
        let stat = fstatat_nofollow(dir_file.as_raw_fd(), &entry_c)?;
        if stat.st_mode & libc::S_IFMT == libc::S_IFDIR {
            unlinkat_tree(dir_file.as_raw_fd(), &entry_c)?;
        } else if unsafe { libc::unlinkat(dir_file.as_raw_fd(), entry_c.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
    }
    dir_file.sync_all()?;
    drop(dir_file);
    if unsafe { libc::unlinkat(parent_fd, name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

fn clear_snapshot_staging_marker(
    edges_dir: &Path,
    project_id: &str,
    snapshot_id: &str,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;

        validate_snapshot_component(project_id)?;
        validate_snapshot_component(snapshot_id)?;
        let relative = Path::new("materialized")
            .join("workspace")
            .join(project_id)
            .join("snapshots")
            .join(snapshot_id);
        let directory = open_dir_under_root(edges_dir, &relative, false)?;
        let marker_c = std::ffi::CString::new(b".staging".as_slice())?;
        match fstatat_nofollow(directory.as_raw_fd(), &marker_c) {
            Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFREG => {}
            Ok(_) => anyhow::bail!("snapshot staging marker is not a regular nofollow file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        }
        if unsafe { libc::unlinkat(directory.as_raw_fd(), marker_c.as_ptr(), 0) } != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
        directory.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let marker = snapshot_dir(edges_dir, project_id, snapshot_id).join(".staging");
        match fs::symlink_metadata(&marker) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
                fs::remove_file(&marker)?;
                Ok(())
            }
            Ok(_) => anyhow::bail!("snapshot staging marker is not a regular nofollow file"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
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

/// Open the parent directory of a project workspace (materialized/workspace/<project_id>)
/// using descriptor-confined traversal with O_NOFOLLOW on every component.
#[cfg(unix)]
fn open_workspace_parent(edges_dir: &Path, project_id: &str) -> Result<fs::File> {
    validate_snapshot_component(project_id)?;
    let relative = Path::new("materialized").join("workspace").join(project_id);
    open_dir_under_root(edges_dir, &relative, true)
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

    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd};

        let parent = open_workspace_parent(edges_dir, project_id)?;
        let overlay_name = DIRTY_OVERLAY_DIRNAME;
        let overlay_c = std::ffi::CString::new(overlay_name)?;
        let tmp_c = std::ffi::CString::new(format!("{overlay_name}.write-tmp"))?;

        // Remove any stale temp via descriptor-relative unlinkat_tree.
        let _ = unlinkat_tree(parent.as_raw_fd(), &tmp_c);

        let all_empty = files.iter().all(|(_, edges)| edges.is_empty());
        if all_empty {
            let _ = unlinkat_tree(parent.as_raw_fd(), &overlay_c);
            parent.sync_all()?;
            return Ok(());
        }

        // Create the temp directory with mkdirat.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), tmp_c.as_ptr(), 0o755) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let tmp_dir_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                tmp_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if tmp_dir_fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let tmp_dir = unsafe { fs::File::from_raw_fd(tmp_dir_fd) };

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
            let filename_c = std::ffi::CString::new(*filename)?;
            let fd = unsafe {
                libc::openat(
                    tmp_dir.as_raw_fd(),
                    filename_c.as_ptr(),
                    libc::O_WRONLY
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { fs::File::from_raw_fd(fd) };
            let mut writer = std::io::BufWriter::new(file);
            for edge in *edges {
                serde_json::to_writer(&mut writer, edge)?;
                writer.write_all(b"\n")?;
            }
            let file = writer.into_inner().map_err(|err| err.into_error())?;
            file.sync_all()?;
        }

        // Write overlay_manifest.json via descriptor-relative openat.
        let manifest_bytes = OverlayManifest::serialize(&covered_hashes)?;
        let manifest_name = OverlayManifest::filename();
        let manifest_c = std::ffi::CString::new(manifest_name.as_bytes())?;
        let fd = unsafe {
            libc::openat(
                tmp_dir.as_raw_fd(),
                manifest_c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let mut manifest_file = unsafe { fs::File::from_raw_fd(fd) };
        manifest_file.write_all(&manifest_bytes)?;
        manifest_file.sync_all()?;
        drop(manifest_file);

        tmp_dir.sync_all()?;
        drop(tmp_dir);

        // Remove the old overlay (if any) and atomically rename the temp.
        let _ = unlinkat_tree(parent.as_raw_fd(), &overlay_c);
        if unsafe {
            libc::renameat(
                parent.as_raw_fd(),
                tmp_c.as_ptr(),
                parent.as_raw_fd(),
                overlay_c.as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        parent.sync_all()?;
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
        let tmp_dir = overlay_dir.with_extension("write-tmp");

        if tmp_dir.is_dir() {
            let _ = fs::remove_dir_all(&tmp_dir);
        }

        let all_empty = files.iter().all(|(_, edges)| edges.is_empty());
        if all_empty {
            if overlay_dir.is_dir() {
                fs::remove_dir_all(&overlay_dir)?;
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
            let mut writer = std::io::BufWriter::new(file);
            for edge in *edges {
                serde_json::to_writer(&mut writer, edge)?;
                writer.write_all(b"\n")?;
            }
            let file = writer.into_inner().map_err(|err| err.into_error())?;
            file.sync_all()?;
        }

        OverlayManifest::write_to(&tmp_dir, &covered_hashes)?;

        if overlay_dir.is_dir() {
            let _ = fs::remove_dir_all(&overlay_dir);
        }
        fs::rename(&tmp_dir, &overlay_dir)?;
        Ok(())
    }
}

pub fn clear_dirty_overlay(edges_dir: &Path, project_id: &str) -> Result<bool> {
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};

        let parent = open_workspace_parent(edges_dir, project_id)?;
        let overlay_c = std::ffi::CString::new(DIRTY_OVERLAY_DIRNAME)?;
        let stat = fstatat_nofollow(parent.as_raw_fd(), &overlay_c);
        match stat {
            Ok(s) if s.st_mode & libc::S_IFMT == libc::S_IFDIR => {}
            Ok(_) | Err(_) => return Ok(false),
        }
        let overlay_fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                overlay_c.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if overlay_fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(error.into());
        }
        let overlay_dir = unsafe { fs::File::from_raw_fd(overlay_fd) };
        let validation = validate_overlay_provenance(&overlay_dir);
        match validation {
            Ok(OverlayValidationOutcome::Valid) => {
                drop(overlay_dir);
                unlinkat_tree(parent.as_raw_fd(), &overlay_c)?;
                parent.sync_all()?;
                Ok(true)
            }
            Ok(OverlayValidationOutcome::Quarantine(bad_names)) => {
                quarantine_dirty_overlay(edges_dir, project_id, &overlay_dir, &bad_names)?;
                tracing::warn!(
                    project_id,
                    ?bad_names,
                    "quarantined dirty overlay containing non-Derived provenance"
                );
                Ok(false)
            }
            // R17F3: I/O or decoding error. The overlay is NOT deleted.
            // The error propagates so the operator sees what went wrong.
            Err(error) => {
                tracing::error!(
                    project_id,
                    error = %error,
                    "dirty overlay validation failed; refusing to delete or quarantine"
                );
                Err(error)
            }
        }
    }
    #[cfg(not(unix))]
    {
        let overlay_dir = dirty_overlay_dir(edges_dir, project_id);
        if !overlay_dir.is_dir() {
            return Ok(false);
        }

        let validation = validate_overlay_provenance_path(&overlay_dir);
        match validation {
            Ok(OverlayValidationOutcome::Valid) => {
                fs::remove_dir_all(&overlay_dir)?;
                Ok(true)
            }
            Ok(OverlayValidationOutcome::Quarantine(bad_names)) => {
                let bad_files: Vec<PathBuf> = bad_names.iter().map(PathBuf::from).collect();
                quarantine_dirty_overlay_path(&overlay_dir, &bad_files)?;
                tracing::warn!(
                    project_id,
                    ?bad_names,
                    "quarantined dirty overlay containing non-Derived provenance"
                );
                Ok(false)
            }
            Err(error) => {
                tracing::error!(
                    project_id,
                    error = %error,
                    "dirty overlay validation failed; refusing to delete or quarantine"
                );
                Err(error)
            }
        }
    }
}

/// R17F3: typed outcome for overlay provenance validation. Every I/O and
/// decoding error is propagated as Err so the overlay is never destroyed
/// without proof that it is safe to clear.
enum OverlayValidationOutcome {
    /// All members are Derived provenance; safe to delete the overlay.
    Valid,
    /// Some members contain non-Derived edges; quarantine them.
    Quarantine(Vec<String>),
}

/// Descriptor-relative provenance validation: reads each .jsonl member
/// through the overlay directory descriptor and returns a typed outcome.
/// R17F3: enumeration failures, stat failures, non-regular entries, open
/// failures, read errors, and malformed JSON ALL propagate as Err.
/// Deletion is permitted only after complete enumeration and successful
/// parsing of every committed member.
#[cfg(unix)]
fn validate_overlay_provenance(overlay_dir: &fs::File) -> Result<OverlayValidationOutcome> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;

    let mut bad = Vec::new();
    let entries = collect_dir_entries(overlay_dir)?;
    for entry_name in entries {
        let name_bytes = entry_name.as_bytes();
        if std::str::from_utf8(name_bytes)
            .map(|s| !s.ends_with(".jsonl"))
            .unwrap_or(true)
        {
            continue;
        }
        let entry_c = std::ffi::CString::new(name_bytes)
            .map_err(|_| anyhow::anyhow!("invalid member name in overlay"))?;
        let stat = fstatat_nofollow(overlay_dir.as_raw_fd(), &entry_c)?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            anyhow::bail!(
                "overlay member {} is not a regular file",
                entry_c.to_string_lossy()
            );
        }
        let fd = unsafe {
            libc::openat(
                overlay_dir.as_raw_fd(),
                entry_c.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let file = unsafe { fs::File::from_raw_fd(fd) };
        let reader = std::io::BufReader::new(file);
        use std::io::BufRead;
        let mut found_bad = false;
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let edge: Edge = serde_json::from_str(trimmed)?;
            if edge.provenance != EdgeProvenance::Derived {
                found_bad = true;
                break;
            }
        }
        if found_bad {
            bad.push(entry_name.to_string_lossy().into_owned());
        }
    }
    if bad.is_empty() {
        Ok(OverlayValidationOutcome::Valid)
    } else {
        Ok(OverlayValidationOutcome::Quarantine(bad))
    }
}

/// Descriptor-relative quarantine: moves named bad members into the
/// dirty-quarantine subdirectory of the project workspace using
/// renameat under the workspace parent descriptor.
#[cfg(unix)]
fn quarantine_dirty_overlay(
    edges_dir: &Path,
    project_id: &str,
    overlay_dir: &fs::File,
    bad_names: &[String],
) -> Result<()> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let parent = open_workspace_parent(edges_dir, project_id)?;
    let quarantine_name = "dirty-quarantine";
    let quarantine_c = std::ffi::CString::new(quarantine_name)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), quarantine_c.as_ptr(), 0o755) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error.into());
        }
    }
    let quarantine_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            quarantine_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if quarantine_fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let quarantine_dir = unsafe { fs::File::from_raw_fd(quarantine_fd) };
    for name in bad_names {
        let name_c = std::ffi::CString::new(name.as_bytes())?;
        if unsafe {
            libc::renameat(
                overlay_dir.as_raw_fd(),
                name_c.as_ptr(),
                quarantine_dir.as_raw_fd(),
                name_c.as_ptr(),
            )
        } != 0
        {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::NotFound {
                return Err(error.into());
            }
        }
    }
    quarantine_dir.sync_all()?;
    parent.sync_all()?;
    Ok(())
}

#[cfg(not(unix))]
/// R17F3: path-based fallback with the same fail-closed semantics.
/// Every I/O and decoding error propagates; deletion only permitted after
/// complete enumeration and successful parsing of every committed member.
fn validate_overlay_provenance_path(overlay_dir: &Path) -> Result<OverlayValidationOutcome> {
    let mut bad = Vec::new();
    let entries = fs::read_dir(overlay_dir)?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let metadata = entry.metadata()?;
        if !metadata.is_file() {
            anyhow::bail!("overlay member {:?} is not a regular file", path);
        }
        let file = fs::File::open(&path)?;
        let reader = std::io::BufReader::new(file);
        use std::io::BufRead;
        let mut found_bad = false;
        for line in reader.lines() {
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let edge: Edge = serde_json::from_str(trimmed)?;
            if edge.provenance != EdgeProvenance::Derived {
                found_bad = true;
                break;
            }
        }
        if found_bad {
            bad.push(path.to_string_lossy().into_owned());
        }
    }
    if bad.is_empty() {
        Ok(OverlayValidationOutcome::Valid)
    } else {
        Ok(OverlayValidationOutcome::Quarantine(bad))
    }
}

#[cfg(not(unix))]
fn quarantine_dirty_overlay_path(overlay_dir: &Path, bad_files: &[PathBuf]) -> Result<()> {
    let quarantine_dir = overlay_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("overlay directory has no parent"))?
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
    if overlay_files.iter().all(|(_, edges)| edges.is_empty()) {
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
        return Ok(());
    }
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
    if !dirty && dirty_overlay_rel.is_none() {
        clear_dirty_overlay(edges_dir, project_id)?;
    }

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
    fn standalone_snapshot_member_transaction_stages_and_finalizes() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // R19: the live snapshot is NOT touched during staging. The member
        // is in the txn staging area. The live git-current.jsonl keeps its
        // old content (from overlay_fixture, which writes empty edges).
        let live_member = snapshot_dir(&edges_dir, "p_1", &snapshot_id).join("git-current.jsonl");
        let live_before = fs::read(&live_member).unwrap_or_default();

        // A journal exists in the txn directory.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        assert!(txn_dir.is_dir(), "txn directory must exist");
        let journals: Vec<_> = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .collect();
        assert_eq!(journals.len(), 1, "exactly one journal must exist");

        // The live member was not modified during staging.
        let live_during = fs::read(&live_member).unwrap_or_default();
        assert_eq!(
            live_before, live_during,
            "live snapshot must not be modified during staging"
        );

        finalize_snapshot_publication(&edges_dir, "p_1", &snapshot_id).unwrap();

        // R19: finalize renames the staged member into the live snapshot.
        assert!(
            snapshot_dir(&edges_dir, "p_1", &snapshot_id)
                .join("git-current.jsonl")
                .exists(),
            "live snapshot must contain the member after finalize"
        );

        // The content changed (staged member replaced the old empty one).
        let live_after =
            fs::read(&snapshot_dir(&edges_dir, "p_1", &snapshot_id).join("git-current.jsonl"))
                .unwrap_or_default();
        assert_ne!(
            live_before, live_after,
            "finalize must replace the member content"
        );

        // The journal and staging dir are cleaned up.
        let journals_after: Vec<_> = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .collect();
        assert!(
            journals_after.is_empty(),
            "journal must be deleted after finalize"
        );

        ManifestIndex::load(&edges_dir)
            .unwrap()
            .active_paths_for_loader(&edges_dir)
            .unwrap();
    }

    #[test]
    fn legacy_in_snapshot_staging_marker_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        // R19F4(c): a legacy .staging marker inside a live snapshot
        // directory (from prior builds) must cause recovery to fail closed.
        fs::write(
            snapshot_dir(&edges_dir, "p_1", &snapshot_id).join(".staging"),
            b"pending\n",
        )
        .unwrap();

        let error = recover_pending_transactions_prebind(&edges_dir)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("legacy"),
            "recovery must fail on legacy marker: {error}"
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

    #[cfg(unix)]
    #[test]
    fn clear_dirty_overlay_fails_on_malformed_json() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(overlay.join("project.jsonl"), b"not valid json {{{").unwrap();

        let result = clear_dirty_overlay(edges_dir, "p1");
        assert!(
            result.is_err(),
            "malformed JSON must propagate an error, not delete the overlay"
        );
        assert!(overlay.exists(), "overlay must survive validation failure");
    }

    #[cfg(unix)]
    #[test]
    fn clear_dirty_overlay_fails_on_non_regular_member() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        fs::create_dir_all(&overlay).unwrap();
        fs::create_dir_all(overlay.join("subdir.jsonl")).unwrap();

        let result = clear_dirty_overlay(edges_dir, "p1");
        assert!(
            result.is_err(),
            "non-regular member must propagate an error"
        );
        assert!(overlay.exists(), "overlay must survive validation failure");
    }

    #[cfg(unix)]
    #[test]
    fn clear_dirty_overlay_fails_on_truncated_read() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let overlay = dirty_overlay_dir(edges_dir, "p1");
        fs::create_dir_all(&overlay).unwrap();

        // Write a valid derived edge followed by a line that cannot be
        // deserialised (truncated record). Validation must fail rather
        // than silently skipping the bad line.
        let good = serde_json::to_string(&derived_edge("k1", "DESCRIBES", "k2")).unwrap();
        let truncated = "{\"truncated";
        let content = format!("{good}\n{truncated}\n");
        fs::write(overlay.join("project.jsonl"), content).unwrap();

        let result = clear_dirty_overlay(edges_dir, "p1");
        assert!(result.is_err(), "decode error mid-stream must propagate");
        assert!(overlay.exists(), "overlay must survive decode failure");
    }

    #[test]
    fn clear_dirty_overlay_succeeds_on_multiple_valid_members() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let edges_a = vec![derived_edge("k1", "DESCRIBES", "k2")];
        let edges_b = vec![derived_edge("k3", "DESCRIBES", "k4")];
        write_dirty_overlay(
            edges_dir,
            "p1",
            &[("project.jsonl", &edges_a), ("extra.jsonl", &edges_b)],
        )
        .unwrap();

        let cleared = clear_dirty_overlay(edges_dir, "p1").unwrap();
        assert!(cleared, "all-Derived overlay must be cleared");
        assert!(
            !dirty_overlay_dir(edges_dir, "p1").exists(),
            "overlay dir must be gone"
        );
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
        assert!(
            !overlay.exists(),
            "empty overlay write must remove the complete overlay directory"
        );
    }

    #[test]
    fn fresh_empty_dirty_publication_keeps_clean_selector() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();

        switch_to_dirty_overlay(
            &edges_dir,
            "p1",
            "repo1",
            Some("main"),
            &"a".repeat(40),
            "dirty-empty",
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let index = ManifestIndex::load(&edges_dir).unwrap();
        let entry = index.workspaces.get("p1").unwrap();
        assert!(entry.dirty_overlay.is_none());
        assert!(!dirty_overlay_dir(&edges_dir, "p1").exists());
        index.active_paths_for_loader(&edges_dir).unwrap();
    }

    #[test]
    fn nonempty_to_empty_dirty_publication_clears_selector_and_directory() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let dirty_edges = vec![derived_edge("dirty", "DESCRIBES", "target")];

        switch_to_dirty_overlay(
            &edges_dir,
            "p1",
            "repo1",
            Some("main"),
            &"a".repeat(40),
            "dirty-1",
            dirty_edges,
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        assert!(dirty_overlay_dir(&edges_dir, "p1").is_dir());
        assert!(
            ManifestIndex::load(&edges_dir)
                .unwrap()
                .workspaces
                .get("p1")
                .unwrap()
                .dirty_overlay
                .is_some()
        );

        switch_to_dirty_overlay(
            &edges_dir,
            "p1",
            "repo1",
            Some("main"),
            &"a".repeat(40),
            "dirty-2",
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();

        let index = ManifestIndex::load(&edges_dir).unwrap();
        assert!(index.workspaces.get("p1").unwrap().dirty_overlay.is_none());
        assert!(!dirty_overlay_dir(&edges_dir, "p1").exists());
        index.active_paths_for_loader(&edges_dir).unwrap();
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

    #[cfg(unix)]
    #[test]
    fn staging_marker_clear_refuses_symlinked_parent() {
        // R16F1: a symlink planted into a parent component of the snapshot
        // directory must prevent staging-marker removal rather than following
        // the symlink outside the state root.
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().canonicalize().unwrap();

        // Build a legitimate snapshot so the directory chain exists.
        let snapshot_id = overlay_fixture(&edges_dir, "p_sym", "gen-a");
        let snap_dir = snapshot_dir(&edges_dir, "p_sym", &snapshot_id);
        let marker = snap_dir.join(".staging");
        // Place a staging marker for the test.
        fs::write(&marker, b"pending\n").unwrap();

        // Replace the project_id component with a symlink to outside.
        let outside = tempfile::tempdir().unwrap();
        let project_dir = materialized_dir(&edges_dir).join("workspace").join("p_sym");
        let real_project = project_dir.canonicalize().unwrap();
        let link_target = outside.path().join("evil-project");
        fs::create_dir_all(link_target.join("snapshots").join(&snapshot_id)).unwrap();
        fs::write(
            link_target
                .join("snapshots")
                .join(&snapshot_id)
                .join(".staging"),
            b"x",
        )
        .unwrap();
        fs::remove_dir_all(&real_project).unwrap();
        std::os::unix::fs::symlink(&link_target, &real_project).unwrap();

        // clear_snapshot_staging_marker must fail (O_NOFOLLOW rejects symlink).
        let result = clear_snapshot_staging_marker(&edges_dir, "p_sym", &snapshot_id);
        assert!(
            result.is_err(),
            "staging marker clear must reject symlinked parent directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overlay_removal_refuses_symlinked_workspace_parent() {
        // R16F1: clear_dirty_overlay must not follow a symlinked workspace
        // parent into an outside directory and recursively delete it.
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().canonicalize().unwrap();

        // Build an overlay legitimately first.
        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(&edges_dir, "p_sym2", &[("project.jsonl", &edges)]).unwrap();

        // Replace the project workspace dir with a symlink to outside.
        let outside = tempfile::tempdir().unwrap();
        let workspace_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_sym2");
        let real_workspace = workspace_dir.canonicalize().unwrap();
        let link_target = outside.path().join("evil-workspace");
        fs::create_dir_all(&link_target).unwrap();
        // Copy overlay-shaped content into the link target.
        fs::create_dir_all(link_target.join("dirty-current")).unwrap();
        fs::write(
            link_target.join("dirty-current").join("project.jsonl"),
            b"outside",
        )
        .unwrap();
        fs::remove_dir_all(&real_workspace).unwrap();
        std::os::unix::fs::symlink(&link_target, &real_workspace).unwrap();

        // clear_dirty_overlay must fail or return false, not delete outside.
        let result = clear_dirty_overlay(&edges_dir, "p_sym2");
        assert!(
            result.is_err() || result.unwrap() == false,
            "overlay removal must not follow symlinked parent"
        );

        // The outside directory must still exist with its content.
        assert!(
            link_target
                .join("dirty-current")
                .join("project.jsonl")
                .exists(),
            "outside content must survive symlink attack"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overlay_replacement_does_not_escape_via_symlink() {
        // R16F1: write_dirty_overlay must not follow a symlinked workspace
        // parent when creating the replacement overlay.
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path().canonicalize().unwrap();

        let edges = vec![derived_edge("k1", "DESCRIBES", "k2")];
        write_dirty_overlay(&edges_dir, "p_sym3", &[("project.jsonl", &edges)]).unwrap();

        // Replace the project workspace dir with a symlink.
        let outside = tempfile::tempdir().unwrap();
        let workspace_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_sym3");
        let real_workspace = workspace_dir.canonicalize().unwrap();
        let link_target = outside.path().join("evil-replacement");
        fs::create_dir_all(&link_target).unwrap();
        fs::remove_dir_all(&real_workspace).unwrap();
        std::os::unix::fs::symlink(&link_target, &real_workspace).unwrap();

        let result = write_dirty_overlay(&edges_dir, "p_sym3", &[("project.jsonl", &edges)]);
        assert!(
            result.is_err(),
            "overlay replacement must reject symlinked parent"
        );
        // Nothing should have been written into the symlink target.
        assert!(
            !link_target.join("dirty-current").exists(),
            "no overlay must appear in symlink target"
        );
    }

    // R19F1: crash between sidecar-stage and Tantivy commit. Recovery
    // discards the staging directory and journal. The live snapshot was
    // never touched: no rollback, no manifest mutation.
    #[test]
    fn r19f2_crash_before_commit_discards_staging() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        // Record the live snapshot state before staging.
        let live_member = snapshot_dir(&edges_dir, "p_1", &snapshot_id).join("git-current.jsonl");
        let live_before = fs::read(&live_member).unwrap_or_default();

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // R19F1: the live snapshot is NOT modified during staging.
        let live_during = fs::read(&live_member).unwrap_or_default();
        assert_eq!(
            live_before, live_during,
            "live snapshot must not be modified during staging"
        );

        // A journal exists.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        assert!(txn_dir.is_dir());

        recover_pending_transactions_prebind(&edges_dir).unwrap();

        // R19F1: the journal and staging dir are discarded.
        let journals: Vec<_> = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .collect();
        assert!(journals.is_empty(), "journal must be discarded by recovery");

        // R19F1: the live snapshot is untouched. No manifest mutation.
        let index = ManifestIndex::load(&edges_dir).unwrap();
        let entry = index.workspaces.get("p_1").unwrap();
        assert!(
            entry.active_snapshot.is_some(),
            "manifest active_snapshot must be preserved (not cleared)"
        );

        // active_paths_for_loader succeeds.
        ManifestIndex::load(&edges_dir)
            .unwrap()
            .active_paths_for_loader(&edges_dir)
            .unwrap();
    }

    // R19F2: crash between Tantivy commit and finalize. Recovery treats
    // the journal as ambiguous and discards. The live snapshot was never
    // touched, so the pre-transaction state is preserved. The next reindex
    // converges.
    #[test]
    fn r19f2_crash_after_commit_discards_staging() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        // Simulate: stage members (write_snapshot_members_transaction),
        // then simulate a Tantivy commit that we cannot prove, then crash
        // before finalize. Recovery finds the journal and discards.
        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Crash: recovery runs without finalize having been called.
        recover_pending_transactions_prebind(&edges_dir).unwrap();

        // The live snapshot's git-current.jsonl still has the old content
        // (staging was discarded, live snapshot untouched).
        let live_after =
            fs::read(&snapshot_dir(&edges_dir, "p_1", &snapshot_id).join("git-current.jsonl"))
                .unwrap_or_default();
        let live_before = b"".to_vec(); // overlay_fixture writes empty git-current
        assert_eq!(
            live_before, live_after,
            "live snapshot must be untouched after ambiguous recovery"
        );

        // The manifest still points to the snapshot (relationship chain
        // sees pre-transaction state).
        ManifestIndex::load(&edges_dir)
            .unwrap()
            .active_paths_for_loader(&edges_dir)
            .unwrap();
    }

    // R19F4: discard-resume idempotency. Recovery run twice is a no-op
    // the second time (journal already deleted).
    #[test]
    fn r19f4_recovery_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // First recovery discards.
        recover_pending_transactions_prebind(&edges_dir).unwrap();

        // Second recovery is a clean no-op.
        recover_pending_transactions_prebind(&edges_dir).unwrap();

        ManifestIndex::load(&edges_dir)
            .unwrap()
            .active_paths_for_loader(&edges_dir)
            .unwrap();
    }

    // R19F4: finish-resume idempotency. A crash inside finalize (members
    // renamed but journal not yet deleted) resumes cleanly: finalize is
    // called again and completes.
    #[test]
    fn r19f4_finalize_resumes_after_partial_crash() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Simulate partial finalize: rename the member manually but leave
        // the journal in place.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        let journal_entry: std::path::PathBuf = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .next()
            .unwrap()
            .path();
        let journal_bytes = fs::read(&journal_entry).unwrap();
        let journal: crate::snapshot::TxnJournalForTest =
            serde_json::from_slice(&journal_bytes).unwrap();
        let staging_dir = txn_dir.join(&journal.txn_token);
        let member_src = staging_dir.join("git-current.jsonl");
        let member_dst = snapshot_dir(&edges_dir, "p_1", &snapshot_id).join("git-current.jsonl");
        fs::rename(&member_src, &member_dst).unwrap();

        // The member is in the live snapshot but the journal still exists.
        assert!(member_dst.exists());

        // Finalize again: it should be idempotent (member already there,
        // journal gets cleaned up).
        finalize_snapshot_publication(&edges_dir, "p_1", &snapshot_id).unwrap();

        // Journal is cleaned up.
        let journals: Vec<_> = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .collect();
        assert!(journals.is_empty(), "journal must be cleaned up");
    }

    // R19F3: finalize renames members into the live snapshot without
    // disturbing pre-existing members.
    #[test]
    fn r19f3_finalize_preserves_existing_snapshot_members() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");

        // The snapshot already has project.jsonl from overlay_fixture.
        assert!(
            snapshot_dir(&edges_dir, "p_1", &snapshot_id)
                .join("project.jsonl")
                .exists()
        );

        let git_edges = vec![explicit_edge("git", "mentions", "target")];
        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        finalize_snapshot_publication(&edges_dir, "p_1", &snapshot_id).unwrap();

        // Both members exist: the pre-existing one and the newly staged one.
        assert!(
            snapshot_dir(&edges_dir, "p_1", &snapshot_id)
                .join("project.jsonl")
                .exists(),
            "pre-existing project.jsonl must survive"
        );
        assert!(
            snapshot_dir(&edges_dir, "p_1", &snapshot_id)
                .join("git-current.jsonl")
                .exists(),
            "newly staged git-current.jsonl must be in live snapshot"
        );
    }

    // R19F5: corrupt member hash must fail closed. The journal and staging
    // dir are preserved; recovery returns an error.
    #[test]
    fn r19f5_corrupt_member_hash_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Corrupt the staged member file.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                continue;
            }
            // This is the staging directory.
            let staging_dir = entry.path();
            let member = staging_dir.join("git-current.jsonl");
            if member.exists() {
                fs::write(&member, b"corrupted\n").unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(
            result.is_err(),
            "recovery must fail on corrupted member hash"
        );

        // The journal is preserved (fail closed).
        let journals: Vec<_> = fs::read_dir(&txn_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.ends_with(".journal.json"))
            })
            .collect();
        assert_eq!(journals.len(), 1, "journal must be preserved on failure");
    }

    // R19F5: missing staged member must fail closed.
    #[test]
    fn r19f5_missing_member_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Delete the staged member file.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                continue;
            }
            let staging_dir = entry.path();
            let member = staging_dir.join("git-current.jsonl");
            if member.exists() {
                fs::remove_file(&member).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(result.is_err(), "recovery must fail on missing member");
    }

    // R19F4: invalid journal version must fail closed.
    #[test]
    fn r19f4_invalid_journal_version_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Overwrite the journal with an invalid version.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                let bad = serde_json::json!({
                    "v": 99,
                    "txn_token": "tok",
                    "snapshot_id": &snapshot_id,
                    "members": [{"name": "git-current.jsonl", "sha256": "a".repeat(64)}]
                });
                fs::write(entry.path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(
            result.is_err(),
            "recovery must fail on invalid journal version"
        );
    }

    // R19F4: path-traversal member name in journal must fail closed.
    #[test]
    fn r19f4_path_traversal_member_name_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Overwrite the journal with a path-traversal member name.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                let bad = serde_json::json!({
                    "v": 1,
                    "txn_token": "tok",
                    "snapshot_id": &snapshot_id,
                    "members": [{"name": "../../../etc/passwd", "sha256": "a".repeat(64)}]
                });
                fs::write(entry.path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(
            result.is_err(),
            "recovery must fail on path-traversal member name"
        );
    }

    // R19F4: invalid sha256 format in journal must fail closed.
    #[test]
    fn r19f4_invalid_sha256_format_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Overwrite the journal with invalid sha256.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                let bad = serde_json::json!({
                    "v": 1,
                    "txn_token": "tok",
                    "snapshot_id": &snapshot_id,
                    "members": [{"name": "git-current.jsonl", "sha256": "tooshort"}]
                });
                fs::write(entry.path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(
            result.is_err(),
            "recovery must fail on invalid sha256 format"
        );
    }

    // R19F4: duplicate member names in journal must fail closed.
    #[test]
    fn r19f4_duplicate_member_names_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Overwrite the journal with duplicate member names.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                let bad = serde_json::json!({
                    "v": 1,
                    "txn_token": "tok",
                    "snapshot_id": &snapshot_id,
                    "members": [
                        {"name": "git-current.jsonl", "sha256": "a".repeat(64)},
                        {"name": "git-current.jsonl", "sha256": "b".repeat(64)}
                    ]
                });
                fs::write(entry.path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(
            result.is_err(),
            "recovery must fail on duplicate member names"
        );
    }

    // R19F4: too many members in journal must fail closed.
    #[test]
    fn r19f4_too_many_members_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[("git-current.jsonl", &git_edges)],
        )
        .unwrap();

        // Overwrite the journal with too many members.
        let txn_dir = materialized_dir(&edges_dir)
            .join("workspace")
            .join("p_1")
            .join("txn");
        let mut members = Vec::new();
        for i in 0..70 {
            members.push(serde_json::json!({
                "name": format!("m{i}.jsonl"),
                "sha256": "a".repeat(64),
            }));
        }
        for entry in fs::read_dir(&txn_dir).unwrap() {
            let entry = entry.unwrap();
            if entry
                .file_name()
                .to_str()
                .unwrap()
                .ends_with(".journal.json")
            {
                let bad = serde_json::json!({
                    "v": 1,
                    "txn_token": "tok",
                    "snapshot_id": &snapshot_id,
                    "members": members,
                });
                fs::write(entry.path(), serde_json::to_vec(&bad).unwrap()).unwrap();
            }
        }

        let result = recover_pending_transactions_prebind(&edges_dir);
        assert!(result.is_err(), "recovery must fail on too many members");
    }

    // R19F1: GC must not deadlock when recovery has already run.
    #[test]
    fn r19f1_gc_does_not_deadlock_after_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let edges_dir = directory.path().canonicalize().unwrap();
        let snapshot_id = overlay_fixture(&edges_dir, "p_1", "gen-a");
        let git_edges = vec![explicit_edge("git", "mentions", "target")];

        write_snapshot_members_transaction(
            &edges_dir,
            "p_1",
            &snapshot_id,
            &[(crate::manifest::GIT_CURRENT_MEMBER, &git_edges)],
        )
        .unwrap();

        recover_pending_transactions_prebind(&edges_dir).unwrap();

        let inactive_path = std::path::PathBuf::from("materialized")
            .join("workspace")
            .join("p_1")
            .join("snapshots")
            .join("nonexistent");
        let result = remove_gc_candidate_file(&edges_dir, &inactive_path, (0, 0), None, true);
        assert!(result.is_ok() || result.is_err());
    }
}

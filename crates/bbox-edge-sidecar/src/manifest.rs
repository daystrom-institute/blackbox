// ---------------------------------------------------------------------------
// Phase 3: Workspace Manifests and Active Loader
// ---------------------------------------------------------------------------
//
// Observed retention gate (phase_3_policy_gate):
//   DECISION: retain observed history indefinitely. Observed lanes (tool
//   edges, provenance) are append-only and do not participate in
//   snapshot/branch mechanics. Storage health reports observed bytes per
//   project and warns when policy caps are exceeded; deletion is explicit.
//
// P1 observed backfill consideration:
//   `bbox_project_register` post-step will retroactively walk transcripts
//   and emit EDITED_FILE/READ_FILE/RAN_BASH edges for the newly registered
//   project (see design/archive/agentic-corpus-followups.md §P1). This
//   will grow observed history for every project that gets registered after
//   initial transcript indexing. Storage health surfaces observed bytes per
//   project so operators can see that growth.
// ---------------------------------------------------------------------------

use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::Result;
use bbox_corpus_core::git_overlay::GitOverlaySelector;
use serde::{Deserialize, Serialize};

pub(crate) const MANIFEST_VERSION: u32 = 1;
const MANIFEST_INDEX_FILENAME: &str = "manifest-index.json";
const OVERLAY_MANIFEST_FILENAME: &str = "overlay_manifest.json";
const OVERLAY_MANIFEST_VERSION: u32 = 1;

// ---------------------------------------------------------------------------
// Per-file overlay manifest
// ---------------------------------------------------------------------------

/// Written alongside the overlay jsonl files so the loader knows which
/// rel_path_hashes the overlay covers. When present, the loader suppresses
/// snapshot edges whose source or target ProjectFile hash is in this set;
/// when absent (legacy overlay), the overlay replaces the whole workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayManifest {
    pub version: u32,
    /// rel_path_hash values whose workspace edges are replaced by the overlay.
    pub covered_rel_path_hashes: Vec<String>,
}

impl OverlayManifest {
    pub fn write_to(overlay_dir: &Path, hashes: &HashSet<String>) -> Result<()> {
        let manifest = OverlayManifest {
            version: OVERLAY_MANIFEST_VERSION,
            covered_rel_path_hashes: hashes.iter().cloned().collect(),
        };
        let path = overlay_dir.join(OVERLAY_MANIFEST_FILENAME);
        let tmp = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp)?;
        serde_json::to_writer(&mut file, &manifest)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp, path)?;
        Ok(())
    }

    pub fn read_from(overlay_dir: &Path) -> Option<Self> {
        let path = overlay_dir.join(OVERLAY_MANIFEST_FILENAME);
        let data = fs::read_to_string(&path).ok()?;
        serde_json::from_str(&data).ok()
    }
}

// ---------------------------------------------------------------------------
// Active path loader view
// ---------------------------------------------------------------------------

/// Describes how to load a sidecar path during EdgeIndex rebuild.
#[derive(Debug)]
pub enum PathLoadMode {
    /// Load all edges without filtering.
    Full,
    /// Load edges, suppressing any that touch a ProjectFile with a
    /// rel_path_hash in the given set (used for snapshot paths when an
    /// overlay covers those files).
    FilteredByHash { suppressed_hashes: HashSet<String> },
}

/// A path and the mode in which the edge loader should process it.
#[derive(Debug)]
pub struct LoadablePath {
    pub path: PathBuf,
    pub file: fs::File,
    pub mode: PathLoadMode,
}

pub fn materialized_dir(edges_dir: &Path) -> PathBuf {
    edges_dir.join("materialized")
}

pub fn workspace_manifest_dir(edges_dir: &Path, project_id: &str) -> PathBuf {
    materialized_dir(edges_dir)
        .join("workspace")
        .join(project_id)
}

pub fn manifest_index_path(edges_dir: &Path) -> PathBuf {
    materialized_dir(edges_dir).join(MANIFEST_INDEX_FILENAME)
}

// ---------------------------------------------------------------------------
// Workspace manifest
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceManifest {
    pub version: u32,
    pub project_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_common_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_worktree_dir: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    pub dirty: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_fingerprint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_snapshot_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_dirty_overlay_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl WorkspaceManifest {
    // kept: public manifest path helper; used by snapshot/manifest tests
    #[allow(dead_code)]
    pub fn manifest_path(edges_dir: &Path, project_id: &str) -> PathBuf {
        workspace_manifest_dir(edges_dir, project_id).join("manifest.json")
    }

    pub fn write_to(edges_dir: &Path, manifest: &Self) -> Result<()> {
        let dir = workspace_manifest_dir(edges_dir, &manifest.project_id);
        fs::create_dir_all(&dir)?;
        let path = dir.join("manifest.json");
        let tmp_path = path.with_extension("json.tmp");
        let mut file = fs::File::create(&tmp_path)?;
        serde_json::to_writer_pretty(&mut file, manifest)?;
        file.sync_all()?;
        drop(file);
        fs::rename(tmp_path, path)?;
        fs::File::open(&dir)?.sync_all()?;
        Ok(())
    }

    pub fn read_from(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path)?;
        let manifest: Self = serde_json::from_str(&data)?;
        if manifest.version != MANIFEST_VERSION {
            anyhow::bail!(
                "manifest version {} != expected {}",
                manifest.version,
                MANIFEST_VERSION
            );
        }
        Ok(manifest)
    }
}

// ---------------------------------------------------------------------------
// Manifest index
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceIndexEntry {
    pub manifest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_snapshot: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dirty_overlay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub repo_materialization: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_source_selector: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_source_generation: Option<String>,
    /// The selected Git current-file overlay, or `None` when the project has
    /// no usable overlay (Phase 3 plan section 10 item 1).
    ///
    /// Additive and defaulted: a manifest written before this field decodes
    /// as "no overlay", which is the correct reading, because a pre-overlay
    /// manifest's `git-current.jsonl` member was written by the in-transaction
    /// Git walk this milestone removes. `active_paths_for_loader` gates that
    /// member on this field, so an absent selector means the member is not
    /// loaded rather than silently trusted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_overlay: Option<GitOverlaySelector>,
    /// Whether this entry's `git-current.jsonl` member is owned by the
    /// overlay lifecycle.
    ///
    /// The two Git-current writers have genuinely different contracts and the
    /// loader cannot tell them apart from the selector alone:
    ///
    /// - Overlay-managed (`true`): the collected activation and the local
    ///   cutback lane. Their activation transaction opens no Git at all, so
    ///   any `git-current.jsonl` in the snapshot directory belongs to a
    ///   PREVIOUS overlay. Absent selector therefore means "do not load".
    /// - Not overlay-managed (`false`, the default): the bridge/local reindex
    ///   lane, which still stages Git current-file edges inside its own
    ///   transaction (plan section 6 item 3 keeps that lane unchanged). Its
    ///   member is always current with its own snapshot, so gating it on a
    ///   selector it never writes would silently delete its commit-file edges
    ///   and break bridge parity.
    ///
    /// Defaulting to `false` is what makes every pre-overlay manifest keep
    /// its existing load behavior.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub git_overlay_managed: bool,
}

/// The one snapshot member the Git overlay owns. A snapshot may carry it
/// from an earlier in-transaction walk (or from a since-cleared overlay);
/// the loader admits it only when the manifest entry selects an overlay.
pub const GIT_CURRENT_MEMBER: &str = "git-current.jsonl";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ManifestIndex {
    pub version: u32,
    pub workspaces: std::collections::BTreeMap<String, WorkspaceIndexEntry>,
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub retirement_tombstones: std::collections::BTreeMap<String, WorkspaceIndexEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
}

impl ManifestIndex {
    pub fn new() -> Self {
        Self {
            version: MANIFEST_VERSION,
            workspaces: std::collections::BTreeMap::new(),
            retirement_tombstones: std::collections::BTreeMap::new(),
            updated_at: None,
        }
    }

    pub fn load(edges_dir: &Path) -> Result<Self> {
        let data = read_manifest_index_confined(edges_dir)?;
        let idx: Self = serde_json::from_slice(&data)?;
        if idx.version != MANIFEST_VERSION {
            anyhow::bail!(
                "manifest-index version {} != expected {}",
                idx.version,
                MANIFEST_VERSION
            );
        }
        Ok(idx)
    }

    pub fn load_or_new(edges_dir: &Path) -> Result<Self> {
        match Self::load(edges_dir) {
            Ok(index) => Ok(index),
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                Ok(Self::new())
            }
            Err(error) => Err(error),
        }
    }

    pub fn write_atomic(&self, edges_dir: &Path) -> Result<()> {
        write_manifest_index_confined(edges_dir, self)
    }

    pub fn upsert_workspace(&mut self, project_id: &str, entry: WorkspaceIndexEntry) {
        self.updated_at = Some(chrono_now_rfc3339());
        self.workspaces.insert(project_id.to_string(), entry);
    }

    pub fn discharge_project_workspace(edges_dir: &Path, project_id: &str) -> Result<bool> {
        let inventory = crate::migration_inventory::capture_project_retirement_inventory(
            edges_dir, project_id,
        )?;
        crate::migration_inventory::discharge_project_retirement_inventory(edges_dir, &inventory)
    }

    pub fn active_materialized_paths(&self, edges_dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            let has_overlay = entry.dirty_overlay.is_some()
                && materialized_dir(edges_dir)
                    .join(entry.dirty_overlay.as_ref().unwrap())
                    .is_dir();

            if has_overlay {
                let overlay_dir =
                    materialized_dir(edges_dir).join(entry.dirty_overlay.as_ref().unwrap());
                append_jsonl_files(&overlay_dir, &mut paths);
            } else if let Some(ref snapshot) = entry.active_snapshot {
                let snapshot_dir = materialized_dir(edges_dir).join(snapshot);
                if snapshot_dir.is_dir() {
                    append_jsonl_files(&snapshot_dir, &mut paths);
                }
            }

            if let Some(ref repo_mat) = entry.repo_materialization {
                let repo_dir = materialized_dir(edges_dir).join(repo_mat);
                if repo_dir.is_dir() {
                    append_jsonl_files(&repo_dir, &mut paths);
                }
            }
            if entry.active_snapshot.is_none() && !has_overlay {
                let managed_dir = edges_dir
                    .join("derived")
                    .join("project")
                    .join(format!("{}.jsonl", project_id));
                if managed_dir.exists() {
                    paths.push(managed_dir);
                }
            }
        }
        paths
    }

    /// Like `active_materialized_paths` but returns `LoadablePath` entries
    /// that carry hash-filter metadata for per-file overlay suppression.
    ///
    /// When a workspace has both a dirty overlay with an `overlay_manifest.json`
    /// AND a clean snapshot, this method yields:
    /// - overlay jsonl files as `Full` (load all edges)
    /// - snapshot jsonl files as `FilteredByHash` (skip edges touching covered hashes)
    ///
    /// Legacy overlays (no overlay_manifest.json) replace the whole workspace
    /// as before (only overlay files returned, `Full`).
    pub fn active_paths_for_loader(&self, edges_dir: &Path) -> Result<Vec<LoadablePath>> {
        let mut result = Vec::new();
        for (project_id, entry) in &self.workspaces {
            validate_workspace_entry_shape(project_id, entry)?;
            confined_regular_file(edges_dir, &entry.manifest)?
                .ok_or_else(|| anyhow::anyhow!("workspace manifest is missing for {project_id}"))?;
            let has_overlay = match entry.dirty_overlay.as_deref() {
                Some(relative) => confined_directory_exists(edges_dir, relative)?,
                None => false,
            };

            if has_overlay {
                let overlay = entry.dirty_overlay.as_deref().unwrap();
                for (path, file) in confined_jsonl_files(edges_dir, overlay)? {
                    result.push(LoadablePath {
                        path,
                        file,
                        mode: PathLoadMode::Full,
                    });
                }

                // Per-file overlay: if overlay_manifest.json present and there
                // is a clean snapshot, load snapshot filtered by covered hashes.
                if let Some(om) = read_overlay_manifest_confined(edges_dir, overlay)? {
                    if let Some(ref snapshot) = entry.active_snapshot {
                        if confined_directory_exists(edges_dir, snapshot)? {
                            let suppressed: HashSet<String> =
                                om.covered_rel_path_hashes.into_iter().collect();
                            if !suppressed.is_empty() {
                                for (path, file) in
                                    confined_snapshot_members(edges_dir, snapshot, entry)?
                                {
                                    result.push(LoadablePath {
                                        path,
                                        file,
                                        mode: PathLoadMode::FilteredByHash {
                                            suppressed_hashes: suppressed.clone(),
                                        },
                                    });
                                }
                            }
                        }
                    }
                }
                // Legacy overlay (no overlay_manifest): snapshot is completely
                // replaced, nothing else to add for this project.
            } else if let Some(ref snapshot) = entry.active_snapshot {
                if confined_directory_exists(edges_dir, snapshot)? {
                    for (path, file) in confined_snapshot_members(edges_dir, snapshot, entry)? {
                        result.push(LoadablePath {
                            path,
                            file,
                            mode: PathLoadMode::Full,
                        });
                    }
                }
            }

            if let Some(ref repo_mat) = entry.repo_materialization {
                if confined_directory_exists(edges_dir, repo_mat)? {
                    for (path, file) in confined_jsonl_files(edges_dir, repo_mat)? {
                        result.push(LoadablePath {
                            path,
                            file,
                            mode: PathLoadMode::Full,
                        });
                    }
                }
            }
            if entry.active_snapshot.is_none() && !has_overlay {
                let managed_rel = format!("derived/project/{project_id}.jsonl");
                if let Some((path, file)) =
                    confined_regular_file_under_root(edges_dir, &managed_rel)?
                {
                    result.push(LoadablePath {
                        path,
                        file,
                        mode: PathLoadMode::Full,
                    });
                }
            }
        }
        Ok(result)
    }

    pub fn protected_materialized_paths(&self, edges_dir: &Path) -> Vec<PathBuf> {
        let mut paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            let has_overlay = entry.dirty_overlay.is_some()
                && materialized_dir(edges_dir)
                    .join(entry.dirty_overlay.as_ref().unwrap())
                    .is_dir();

            if has_overlay {
                let overlay_dir =
                    materialized_dir(edges_dir).join(entry.dirty_overlay.as_ref().unwrap());
                append_jsonl_files(&overlay_dir, &mut paths);
            }

            if let Some(ref snapshot) = entry.active_snapshot {
                let snapshot_dir = materialized_dir(edges_dir).join(snapshot);
                if snapshot_dir.is_dir() {
                    append_jsonl_files(&snapshot_dir, &mut paths);
                }
            }

            if let Some(ref repo_mat) = entry.repo_materialization {
                let repo_dir = materialized_dir(edges_dir).join(repo_mat);
                if repo_dir.is_dir() {
                    append_jsonl_files(&repo_dir, &mut paths);
                }
            }
            if entry.active_snapshot.is_none() && !has_overlay {
                let managed_dir = edges_dir
                    .join("derived")
                    .join("project")
                    .join(format!("{}.jsonl", project_id));
                if managed_dir.exists() {
                    paths.push(managed_dir);
                }
            }
        }
        paths
    }

    pub fn validate(&self, edges_dir: &Path) -> ManifestValidation {
        let mut missing_manifests = Vec::new();
        let mut missing_paths = Vec::new();
        for (project_id, entry) in &self.workspaces {
            if let Err(error) = validate_workspace_entry_shape(project_id, entry) {
                missing_paths.push(format!("{project_id}: {error}"));
                continue;
            }
            if !confined_regular_file(edges_dir, &entry.manifest).is_ok_and(|file| file.is_some()) {
                missing_manifests.push(project_id.clone());
                continue;
            }
            if let Some(ref snapshot) = entry.active_snapshot {
                if !confined_directory_exists(edges_dir, snapshot).unwrap_or(false) {
                    missing_paths.push(snapshot.clone());
                }
            }
            if let Some(ref overlay) = entry.dirty_overlay {
                if !confined_directory_exists(edges_dir, overlay).unwrap_or(false) {
                    missing_paths.push(overlay.clone());
                }
            }
            if let Some(ref repo_mat) = entry.repo_materialization {
                if !confined_directory_exists(edges_dir, repo_mat).unwrap_or(false) {
                    missing_paths.push(repo_mat.clone());
                }
            }
        }
        if missing_manifests.is_empty() && missing_paths.is_empty() {
            ManifestValidation::Valid
        } else {
            ManifestValidation::Invalid {
                missing_manifests,
                missing_paths,
            }
        }
    }
}

const MAX_MANIFEST_INDEX_BYTES: usize = 16 * 1024 * 1024;

#[cfg(unix)]
fn open_materialized_dir(edges_dir: &Path, create: bool) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::fs::OpenOptionsExt;

    if create {
        fs::create_dir_all(edges_dir)?;
    }
    let root = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(edges_dir)?;
    let name = std::ffi::CString::new("materialized").unwrap();
    if create {
        let status = unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o755) };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() != std::io::ErrorKind::AlreadyExists {
                return Err(error.into());
            }
        }
    }
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn read_manifest_index_confined(edges_dir: &Path) -> Result<Vec<u8>> {
    use std::io::Read;
    use std::os::fd::{AsRawFd, FromRawFd};

    let dir = open_materialized_dir(edges_dir, false)?;
    let name = std::ffi::CString::new("manifest-index.json").unwrap();
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() as usize > MAX_MANIFEST_INDEX_BYTES {
        anyhow::bail!("manifest-index is not a bounded regular file");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_MANIFEST_INDEX_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        anyhow::bail!("manifest-index changed while being read");
    }
    Ok(bytes)
}

#[cfg(unix)]
fn write_manifest_index_confined(edges_dir: &Path, index: &ManifestIndex) -> Result<()> {
    use std::io::Write;
    use std::os::fd::{AsRawFd, FromRawFd};

    let bytes = serde_json::to_vec_pretty(index)?;
    if bytes.len() > MAX_MANIFEST_INDEX_BYTES {
        anyhow::bail!("manifest-index exceeds its byte limit");
    }
    let dir = open_materialized_dir(edges_dir, true)?;
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_name = format!(".manifest-index.{}.{}.tmp", std::process::id(), sequence);
    let temp = std::ffi::CString::new(temp_name.as_bytes())?;
    let target = std::ffi::CString::new("manifest-index.json").unwrap();
    let fd = unsafe {
        libc::openat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut file = unsafe { fs::File::from_raw_fd(fd) };
    if let Err(error) = file.write_all(&bytes).and_then(|_| file.sync_all()) {
        unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    drop(file);
    if unsafe {
        libc::renameat(
            dir.as_raw_fd(),
            temp.as_ptr(),
            dir.as_raw_fd(),
            target.as_ptr(),
        )
    } != 0
    {
        let error = std::io::Error::last_os_error();
        unsafe { libc::unlinkat(dir.as_raw_fd(), temp.as_ptr(), 0) };
        return Err(error.into());
    }
    dir.sync_all()?;
    Ok(())
}

const MAX_ACTIVE_MATERIALIZATION_FILES: usize = 100_000;
const MAX_OVERLAY_MANIFEST_BYTES: usize = 1024 * 1024;

fn validate_workspace_entry_shape(project_id: &str, entry: &WorkspaceIndexEntry) -> Result<()> {
    validate_single_component(project_id, "project id")?;
    let expected_manifest = format!("workspace/{project_id}/manifest.json");
    if entry.manifest != expected_manifest {
        anyhow::bail!(
            "workspace manifest path `{}` does not match writer path `{expected_manifest}`",
            entry.manifest
        );
    }
    if let Some(snapshot) = entry.active_snapshot.as_deref() {
        validate_snapshot_path(project_id, snapshot)?;
    }
    if let Some(overlay) = entry.dirty_overlay.as_deref()
        && overlay != crate::snapshot::dirty_overlay_rel(project_id)
    {
        anyhow::bail!("workspace dirty overlay path `{overlay}` is not writer-normalized");
    }
    if let Some(repo) = entry.repo_materialization.as_deref() {
        validate_relative_path(repo)?;
    }
    Ok(())
}

fn validate_snapshot_path(project_id: &str, relative: &str) -> Result<()> {
    let components = normal_components(relative)?;
    if components.len() != 4
        || components[0] != "workspace"
        || components[1] != project_id
        || components[2] != "snapshots"
    {
        anyhow::bail!("workspace snapshot path `{relative}` is not writer-normalized");
    }
    validate_single_component(&components[3], "snapshot id")
}

fn validate_relative_path(relative: &str) -> Result<()> {
    normal_components(relative).map(|_| ())
}

fn validate_single_component(value: &str, label: &str) -> Result<()> {
    if value.is_empty()
        || Path::new(value).components().count() != 1
        || !matches!(
            Path::new(value).components().next(),
            Some(std::path::Component::Normal(_))
        )
    {
        anyhow::bail!("{label} is not a non-empty normal path component");
    }
    Ok(())
}

fn normal_components(relative: &str) -> Result<Vec<String>> {
    let path = Path::new(relative);
    if relative.is_empty() || path.is_absolute() {
        anyhow::bail!("workspace path is not a non-empty relative path");
    }
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            std::path::Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("workspace path is not UTF-8"))?;
                if value.is_empty() {
                    anyhow::bail!("workspace path contains an empty component");
                }
                components.push(value.to_string());
            }
            _ => anyhow::bail!("workspace path contains a non-normal component"),
        }
    }
    if components.is_empty() {
        anyhow::bail!("workspace path has no components");
    }
    Ok(components)
}

#[cfg(unix)]
fn open_confined_directory(base: &fs::File, relative: &str) -> Result<fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let mut current = base.try_clone()?;
    for component in normal_components(relative)? {
        let component = std::ffi::CString::new(component)?;
        let fd = unsafe {
            libc::openat(
                current.as_raw_fd(),
                component.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        current = unsafe { fs::File::from_raw_fd(fd) };
    }
    Ok(current)
}

#[cfg(unix)]
fn open_confined_regular(base: &fs::File, relative: &str) -> Result<Option<fs::File>> {
    use std::os::fd::{AsRawFd, FromRawFd};

    let components = normal_components(relative)?;
    let (leaf, parents) = components.split_last().unwrap();
    let parent = if parents.is_empty() {
        base.try_clone()?
    } else {
        open_confined_directory(base, &parents.join("/"))?
    };
    let leaf = std::ffi::CString::new(leaf.as_str())?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            leaf.as_ptr(),
            libc::O_RDONLY | libc::O_NONBLOCK | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error.into());
    }
    let file = unsafe { fs::File::from_raw_fd(fd) };
    if !file.metadata()?.is_file() {
        anyhow::bail!("workspace member is not a regular file");
    }
    Ok(Some(file))
}

#[cfg(unix)]
fn read_directory_names(directory: &fs::File) -> Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;

    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = Vec::new();
    loop {
        set_readdir_errno(0);
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let error = readdir_errno();
            unsafe { libc::closedir(stream) };
            if error != 0 {
                return Err(std::io::Error::from_raw_os_error(error).into());
            }
            break;
        }
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
            if names.len() > MAX_ACTIVE_MATERIALIZATION_FILES {
                unsafe { libc::closedir(stream) };
                anyhow::bail!("workspace materialization exceeds its file limit");
            }
        }
    }
    Ok(names)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn set_readdir_errno(value: libc::c_int) {
    unsafe { *libc::__errno_location() = value };
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn set_readdir_errno(value: libc::c_int) {
    unsafe { *libc::__error() = value };
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn readdir_errno() -> libc::c_int {
    unsafe { *libc::__errno_location() }
}

#[cfg(any(target_os = "macos", target_os = "ios"))]
fn readdir_errno() -> libc::c_int {
    unsafe { *libc::__error() }
}

#[cfg(unix)]
fn confined_directory_exists(edges_dir: &Path, relative: &str) -> Result<bool> {
    let materialized = open_materialized_dir(edges_dir, false)?;
    match open_confined_directory(&materialized, relative) {
        Ok(_) => Ok(true),
        Err(error)
            if error.chain().any(|cause| {
                cause
                    .downcast_ref::<io::Error>()
                    .is_some_and(|io| io.kind() == io::ErrorKind::NotFound)
            }) =>
        {
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn confined_regular_file(edges_dir: &Path, relative: &str) -> Result<Option<(PathBuf, fs::File)>> {
    let materialized = open_materialized_dir(edges_dir, false)?;
    Ok(open_confined_regular(&materialized, relative)?
        .map(|file| (materialized_dir(edges_dir).join(relative), file)))
}

#[cfg(unix)]
fn confined_regular_file_under_root(
    edges_dir: &Path,
    relative: &str,
) -> Result<Option<(PathBuf, fs::File)>> {
    use std::os::unix::fs::OpenOptionsExt;

    let root = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(edges_dir)?;
    Ok(open_confined_regular(&root, relative)?.map(|file| (edges_dir.join(relative), file)))
}

#[cfg(unix)]
fn confined_jsonl_files(edges_dir: &Path, relative: &str) -> Result<Vec<(PathBuf, fs::File)>> {
    let materialized = open_materialized_dir(edges_dir, false)?;
    let directory = open_confined_directory(&materialized, relative)?;
    let mut files = Vec::new();
    for name in read_directory_names(&directory)? {
        if Path::new(&name)
            .extension()
            .and_then(|value| value.to_str())
            != Some("jsonl")
        {
            continue;
        }
        let name = name
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("workspace member name is not UTF-8"))?;
        let file = open_confined_regular(&directory, name)?
            .ok_or_else(|| anyhow::anyhow!("workspace member disappeared during enumeration"))?;
        files.push((materialized_dir(edges_dir).join(relative).join(name), file));
    }
    Ok(files)
}

#[cfg(unix)]
fn read_overlay_manifest_confined(
    edges_dir: &Path,
    overlay: &str,
) -> Result<Option<OverlayManifest>> {
    use std::io::Read;

    let materialized = open_materialized_dir(edges_dir, false)?;
    let directory = open_confined_directory(&materialized, overlay)?;
    let Some(mut file) = open_confined_regular(&directory, OVERLAY_MANIFEST_FILENAME)? else {
        return Ok(None);
    };
    let metadata = file.metadata()?;
    if metadata.len() > MAX_OVERLAY_MANIFEST_BYTES as u64 {
        anyhow::bail!("overlay manifest exceeds its byte limit");
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.by_ref()
        .take(MAX_OVERLAY_MANIFEST_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 != metadata.len() {
        anyhow::bail!("overlay manifest changed while being read");
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

#[cfg(unix)]
fn confined_snapshot_members(
    edges_dir: &Path,
    snapshot: &str,
    entry: &WorkspaceIndexEntry,
) -> Result<Vec<(PathBuf, fs::File)>> {
    let overlay_admits_git_current = !entry.git_overlay_managed
        || entry.git_overlay.as_ref().is_some_and(|overlay| {
            entry
                .code_source_generation
                .as_deref()
                .is_some_and(|generation| overlay.matches_code_generation(generation))
        });
    let mut files = confined_jsonl_files(edges_dir, snapshot)?;
    if !overlay_admits_git_current {
        files.retain(|(path, _)| {
            path.file_name().and_then(|name| name.to_str()) != Some(GIT_CURRENT_MEMBER)
        });
    }
    Ok(files)
}

#[cfg(not(unix))]
fn confined_directory_exists(edges_dir: &Path, relative: &str) -> Result<bool> {
    validate_relative_path(relative)?;
    Ok(materialized_dir(edges_dir).join(relative).is_dir())
}

#[cfg(not(unix))]
fn confined_regular_file(edges_dir: &Path, relative: &str) -> Result<Option<(PathBuf, fs::File)>> {
    validate_relative_path(relative)?;
    let path = materialized_dir(edges_dir).join(relative);
    match fs::File::open(&path) {
        Ok(file) if file.metadata()?.is_file() => Ok(Some((path, file))),
        Ok(_) => anyhow::bail!("workspace member is not a regular file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn confined_regular_file_under_root(
    edges_dir: &Path,
    relative: &str,
) -> Result<Option<(PathBuf, fs::File)>> {
    validate_relative_path(relative)?;
    let path = edges_dir.join(relative);
    match fs::File::open(&path) {
        Ok(file) if file.metadata()?.is_file() => Ok(Some((path, file))),
        Ok(_) => anyhow::bail!("workspace member is not a regular file"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn confined_jsonl_files(edges_dir: &Path, relative: &str) -> Result<Vec<(PathBuf, fs::File)>> {
    validate_relative_path(relative)?;
    let directory = materialized_dir(edges_dir).join(relative);
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) == Some("jsonl") {
            let file = fs::File::open(&path)?;
            if !file.metadata()?.is_file() {
                anyhow::bail!("workspace member is not a regular file");
            }
            files.push((path, file));
        }
    }
    Ok(files)
}

#[cfg(not(unix))]
fn read_overlay_manifest_confined(
    edges_dir: &Path,
    overlay: &str,
) -> Result<Option<OverlayManifest>> {
    validate_relative_path(overlay)?;
    let path = materialized_dir(edges_dir)
        .join(overlay)
        .join(OVERLAY_MANIFEST_FILENAME);
    match fs::read(path) {
        Ok(bytes) if bytes.len() <= MAX_OVERLAY_MANIFEST_BYTES => {
            Ok(Some(serde_json::from_slice(&bytes)?))
        }
        Ok(_) => anyhow::bail!("overlay manifest exceeds its byte limit"),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn confined_snapshot_members(
    edges_dir: &Path,
    snapshot: &str,
    entry: &WorkspaceIndexEntry,
) -> Result<Vec<(PathBuf, fs::File)>> {
    let overlay_admits_git_current = !entry.git_overlay_managed
        || entry.git_overlay.as_ref().is_some_and(|overlay| {
            entry
                .code_source_generation
                .as_deref()
                .is_some_and(|generation| overlay.matches_code_generation(generation))
        });
    let mut files = confined_jsonl_files(edges_dir, snapshot)?;
    if !overlay_admits_git_current {
        files.retain(|(path, _)| {
            path.file_name().and_then(|name| name.to_str()) != Some(GIT_CURRENT_MEMBER)
        });
    }
    Ok(files)
}

#[cfg(not(unix))]
fn read_manifest_index_confined(edges_dir: &Path) -> Result<Vec<u8>> {
    let bytes = fs::read(manifest_index_path(edges_dir))?;
    if bytes.len() > MAX_MANIFEST_INDEX_BYTES {
        anyhow::bail!("manifest-index exceeds its byte limit");
    }
    Ok(bytes)
}

#[cfg(not(unix))]
fn write_manifest_index_confined(edges_dir: &Path, index: &ManifestIndex) -> Result<()> {
    let dir = materialized_dir(edges_dir);
    fs::create_dir_all(&dir)?;
    let bytes = serde_json::to_vec_pretty(index)?;
    if bytes.len() > MAX_MANIFEST_INDEX_BYTES {
        anyhow::bail!("manifest-index exceeds its byte limit");
    }
    let temp = dir.join(format!(".manifest-index.{}.tmp", std::process::id()));
    fs::write(&temp, bytes)?;
    fs::rename(temp, manifest_index_path(edges_dir))?;
    fs::File::open(&dir)?.sync_all()?;
    Ok(())
}

fn append_jsonl_files(dir: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            paths.push(path);
        }
    }
}

// ---------------------------------------------------------------------------
// Fallback reason for active loader
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestFallbackReason {
    MissingNotMigrated,
    Corrupt {
        error: String,
    },
    Stale {
        missing_manifests: Vec<String>,
        missing_paths: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Manifest validation result
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum ManifestValidation {
    Valid,
    Invalid {
        missing_manifests: Vec<String>,
        missing_paths: Vec<String>,
    },
}

// ---------------------------------------------------------------------------
// Try-load with fallback classification
// ---------------------------------------------------------------------------

pub fn try_load_manifest_index(edges_dir: &Path) -> Result<ManifestIndex, ManifestFallbackReason> {
    let path = manifest_index_path(edges_dir);
    if !path.exists() {
        return Err(ManifestFallbackReason::MissingNotMigrated);
    }
    match ManifestIndex::load(edges_dir) {
        Ok(idx) => match idx.validate(edges_dir) {
            ManifestValidation::Valid => Ok(idx),
            ManifestValidation::Invalid {
                missing_manifests,
                missing_paths,
            } => Err(ManifestFallbackReason::Stale {
                missing_manifests,
                missing_paths,
            }),
        },
        Err(e) => Err(ManifestFallbackReason::Corrupt {
            error: e.to_string(),
        }),
    }
}

pub(crate) fn chrono_now_rfc3339() -> String {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_index_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj1234",
            WorkspaceIndexEntry {
                manifest: "workspace/proj1234/manifest.json".into(),
                active_snapshot: Some("workspace/proj1234/snapshots/head-abc".into()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let loaded = ManifestIndex::load(edges_dir).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(loaded.workspaces.len(), 1);
        assert_eq!(
            loaded.workspaces["proj1234"].manifest,
            "workspace/proj1234/manifest.json"
        );

        let snapshot = materialized_dir(edges_dir).join("workspace/proj1234/snapshots/head-abc");
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(workspace_manifest_dir(edges_dir, "proj1234")).unwrap();
        assert!(ManifestIndex::discharge_project_workspace(edges_dir, "proj1234").unwrap());
        assert!(!ManifestIndex::discharge_project_workspace(edges_dir, "proj1234").unwrap());
        assert!(!snapshot.exists());
        assert!(
            !ManifestIndex::load(edges_dir)
                .unwrap()
                .workspaces
                .contains_key("proj1234")
        );
    }

    #[cfg(unix)]
    #[test]
    fn manifest_index_load_refuses_symlinked_authority_file() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::create_dir_all(materialized_dir(dir.path())).unwrap();
        std::os::unix::fs::symlink(outside.path(), manifest_index_path(dir.path())).unwrap();
        assert!(ManifestIndex::load(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn manifest_index_write_refuses_symlinked_materialized_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), materialized_dir(dir.path())).unwrap();
        assert!(ManifestIndex::new().write_atomic(dir.path()).is_err());
        assert!(!outside.path().join(MANIFEST_INDEX_FILENAME).exists());
    }

    #[test]
    fn manifest_index_missing_is_missing_not_migrated() {
        let dir = tempfile::tempdir().unwrap();
        let result = try_load_manifest_index(dir.path());
        assert_eq!(
            result.unwrap_err(),
            ManifestFallbackReason::MissingNotMigrated
        );
    }

    #[test]
    fn manifest_index_corrupt_json_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mat_dir = materialized_dir(dir.path());
        fs::create_dir_all(&mat_dir).unwrap();
        fs::write(manifest_index_path(dir.path()), b"not json{{{").unwrap();

        let result = try_load_manifest_index(dir.path());
        match result.unwrap_err() {
            ManifestFallbackReason::Corrupt { .. } => {}
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }

    #[test]
    fn manifest_index_stale_missing_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj_missing",
            WorkspaceIndexEntry {
                manifest: "workspace/proj_missing/manifest.json".into(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        idx.write_atomic(edges_dir).unwrap();

        let result = try_load_manifest_index(edges_dir);
        match result.unwrap_err() {
            ManifestFallbackReason::Stale {
                missing_manifests, ..
            } => {
                assert!(missing_manifests.contains(&"proj_missing".to_string()));
            }
            other => panic!("expected Stale, got {:?}", other),
        }
    }

    #[test]
    fn workspace_manifest_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let manifest = WorkspaceManifest {
            version: 1,
            project_id: "proj1234".into(),
            repo_id: Some("repo_abcd".into()),
            canonical_path: Some("/home/me/repo".into()),
            git_common_dir: None,
            git_worktree_dir: None,
            branch: Some("main".into()),
            head_sha: Some("abc123".into()),
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("head-abc123".into()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        WorkspaceManifest::write_to(dir.path(), &manifest).unwrap();

        let path = WorkspaceManifest::manifest_path(dir.path(), "proj1234");
        let loaded = WorkspaceManifest::read_from(&path).unwrap();
        assert_eq!(loaded, manifest);
    }

    #[test]
    fn active_materialized_paths_includes_managed_project_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let managed_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&managed_dir).unwrap();
        fs::write(managed_dir.join("proj1234.jsonl"), b"{}").unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "proj1234",
            WorkspaceIndexEntry {
                manifest: "workspace/proj1234/manifest.json".into(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("proj1234.jsonl")),
            "managed project sidecar must be in active paths"
        );
    }

    #[test]
    fn active_loader_skips_inactive_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let active_snap = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-active");
        fs::create_dir_all(&active_snap).unwrap();
        let active_edge = r#"{"source":"knowledge:k1","kind":"DESCRIBES","target":"knowledge:k2","provenance":"explicit","confidence":"exact","metadata":{}}"#;
        fs::write(active_snap.join("project.jsonl"), active_edge).unwrap();

        let inactive_snap = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-old-branch");
        fs::create_dir_all(&inactive_snap).unwrap();
        let inactive_edge = r#"{"source":"knowledge:stale","kind":"DESCRIBES","target":"knowledge:k3","provenance":"explicit","confidence":"exact","metadata":{}}"#;
        fs::write(inactive_snap.join("project.jsonl"), inactive_edge).unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/head-active".into()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-active")),
            "active snapshot must be in paths"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-old-branch")),
            "inactive snapshot must NOT be in paths"
        );
    }

    #[test]
    fn active_paths_excludes_managed_sidecar_when_snapshot_exists() {
        let dir = tempfile::tempdir().unwrap();
        let edges_dir = dir.path();

        let managed_dir = edges_dir.join("derived").join("project");
        fs::create_dir_all(&managed_dir).unwrap();
        fs::write(managed_dir.join("p1.jsonl"), b"{}").unwrap();

        let snap_dir = materialized_dir(edges_dir)
            .join("workspace")
            .join("p1")
            .join("snapshots")
            .join("head-abc");
        fs::create_dir_all(&snap_dir).unwrap();
        fs::write(snap_dir.join("project.jsonl"), b"{}").unwrap();

        let mut idx = ManifestIndex::new();
        idx.upsert_workspace(
            "p1",
            WorkspaceIndexEntry {
                manifest: "workspace/p1/manifest.json".into(),
                active_snapshot: Some("workspace/p1/snapshots/head-abc".into()),
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );

        let paths = idx.active_materialized_paths(edges_dir);
        assert!(
            paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("head-abc")),
            "active snapshot must be in paths"
        );
        assert!(
            !paths
                .iter()
                .any(|p| p.to_str().unwrap().contains("derived/project/p1.jsonl")),
            "managed sidecar must NOT be in paths when snapshot exists"
        );
    }

    #[test]
    fn manifest_version_mismatch_is_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let mat_dir = materialized_dir(dir.path());
        fs::create_dir_all(&mat_dir).unwrap();
        let bad = r#"{"version":99,"workspaces":{},"updated_at":null}"#;
        fs::write(manifest_index_path(dir.path()), bad).unwrap();

        let result = try_load_manifest_index(dir.path());
        match result.unwrap_err() {
            ManifestFallbackReason::Corrupt { error } => {
                assert!(error.contains("version"));
            }
            other => panic!("expected Corrupt, got {:?}", other),
        }
    }

    fn local_workspace_entry(project_id: &str, snapshot: &str) -> WorkspaceIndexEntry {
        WorkspaceIndexEntry {
            manifest: format!("workspace/{project_id}/manifest.json"),
            active_snapshot: Some(snapshot.to_string()),
            dirty_overlay: None,
            repo_materialization: None,
            code_source_selector: Some(bbox_code_source::local_selector(project_id)),
            code_source_generation: Some("local".to_string()),
            git_overlay: None,
            git_overlay_managed: false,
        }
    }

    #[test]
    fn active_loader_rejects_non_writer_workspace_shapes_for_local_entries() {
        let dir = tempfile::tempdir().unwrap();
        let mut index = ManifestIndex::new();
        index
            .workspaces
            .insert("p1".to_string(), local_workspace_entry("p1", "../outside"));
        assert!(index.active_paths_for_loader(dir.path()).is_err());

        let mut entry = local_workspace_entry("p1", "workspace/p1/snapshots/local-a");
        entry.manifest = "/tmp/outside.json".to_string();
        index.workspaces.insert("p1".to_string(), entry);
        assert!(index.active_paths_for_loader(dir.path()).is_err());

        let mut entry = local_workspace_entry("p1", "workspace/p1/snapshots/local-a");
        entry.dirty_overlay = Some("workspace/p1/../outside".to_string());
        index.workspaces.insert("p1".to_string(), entry);
        assert!(index.active_paths_for_loader(dir.path()).is_err());

        let mut entry = local_workspace_entry("p1", "workspace/p1/snapshots/local-a");
        entry.repo_materialization = Some("../../outside".to_string());
        index.workspaces.insert("p1".to_string(), entry);
        assert!(index.active_paths_for_loader(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn active_loader_rejects_symlinked_snapshot_directory() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("external.jsonl"), b"{}\n").unwrap();
        let workspace = workspace_manifest_dir(dir.path(), "p1");
        fs::create_dir_all(workspace.join("snapshots")).unwrap();
        fs::write(workspace.join("manifest.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), workspace.join("snapshots").join("local-a"))
            .unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "p1".to_string(),
            local_workspace_entry("p1", "workspace/p1/snapshots/local-a"),
        );

        assert!(index.active_paths_for_loader(dir.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn active_loader_rejects_symlinked_jsonl_member() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        let workspace = workspace_manifest_dir(dir.path(), "p1");
        let snapshot = workspace.join("snapshots").join("local-a");
        fs::create_dir_all(&snapshot).unwrap();
        fs::write(workspace.join("manifest.json"), b"{}").unwrap();
        std::os::unix::fs::symlink(outside.path(), snapshot.join("external.jsonl")).unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "p1".to_string(),
            local_workspace_entry("p1", "workspace/p1/snapshots/local-a"),
        );

        assert!(index.active_paths_for_loader(dir.path()).is_err());
    }
}

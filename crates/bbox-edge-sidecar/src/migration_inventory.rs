//! Strict migration snapshot of edge workspace manifests and selectors.
//!
//! The capture runs behind the manifest owner's coordinator and performs no
//! create, repair, activation, or fallback-to-empty behavior.

use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(unix)]
use std::os::fd::AsRawFd;

use crate::manifest::{
    MANIFEST_VERSION, ManifestIndex, WorkspaceIndexEntry, WorkspaceManifest, chrono_now_rfc3339,
    manifest_index_path, materialized_dir, workspace_manifest_dir,
};
use crate::snapshot::with_manifest_coordinator;

const SNAPSHOT_VERSION_V1: u32 = 1;
const SCHEMA_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.schema.v1\0";
const ROW_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.workspace-rows.v1\0";
const SOURCE_HASH_DOMAIN: &[u8] = b"blackbox.edge-manifest.source.v1\0";
pub const RETIREMENT_INVENTORY_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeReceiptCloseoutEvidence {
    pub commitment: String,
    pub snapshot: String,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EdgeRetirementInventory {
    pub version: u32,
    pub project_id: String,
    pub relative_paths: Vec<String>,
    pub receipt_bindings: std::collections::BTreeMap<String, String>,
    pub receipt_closeouts: Vec<EdgeReceiptCloseoutEvidence>,
}

pub fn capture_project_retirement_inventory(
    edges_dir: &Path,
    project_id: &str,
) -> Result<EdgeRetirementInventory> {
    if project_id.is_empty()
        || Path::new(project_id)
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        anyhow::bail!("edge retirement project id is invalid");
    }
    match fs::symlink_metadata(edges_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(EdgeRetirementInventory {
                version: RETIREMENT_INVENTORY_VERSION,
                project_id: project_id.to_string(),
                relative_paths: Vec::new(),
                receipt_bindings: std::collections::BTreeMap::new(),
                receipt_closeouts: Vec::new(),
            });
        }
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            anyhow::bail!("edge retirement root is not a strict directory");
        }
        Ok(_) => {}
    }
    let anchored = AnchoredEdgeRoot::open(edges_dir)?;

    let mut paths = std::collections::BTreeSet::new();
    for relative in [
        PathBuf::from(format!("{project_id}.jsonl")),
        PathBuf::from("explicit").join(format!("{project_id}.jsonl")),
        PathBuf::from("observed").join(format!("{project_id}.jsonl")),
    ] {
        insert_existing_retirement_path(&anchored, relative, &mut paths)?;
    }
    if anchored.path_exists(Path::new("derived"))? {
        for namespace in anchored.list_directory(Path::new("derived"))? {
            let namespace_path = PathBuf::from("derived").join(&namespace);
            anchored.require_directory(&namespace_path)?;
            insert_existing_retirement_path(
                &anchored,
                namespace_path.join(format!("{project_id}.jsonl")),
                &mut paths,
            )?;
        }
    }

    let mut receipt_bindings = std::collections::BTreeMap::new();
    let mut receipt_closeouts = Vec::new();
    if anchored.path_exists(Path::new("materialized/manifest-index.json"))? {
        let index = ManifestIndex::load(edges_dir)?;
        let active = index.workspaces.get(project_id);
        let tombstone = index.retirement_tombstones.get(project_id);
        if active.is_some() && tombstone.is_some() {
            anyhow::bail!("edge workspace has both an active entry and a retirement tombstone");
        }
        if let Some(entry) = active.or(tombstone) {
            for relative in workspace_entry_paths(edges_dir, project_id, entry)? {
                insert_existing_retirement_path(&anchored, relative, &mut paths)?;
            }
        }
        let snapshot_prefix = format!("workspace/{project_id}/snapshots/");
        receipt_bindings.extend(
            index
                .receipt_managed_snapshots
                .iter()
                .filter(|(snapshot, _)| snapshot.starts_with(&snapshot_prefix))
                .map(|(snapshot, digest)| (snapshot.clone(), digest.clone())),
        );
        receipt_closeouts.extend(
            index
                .receipt_closeouts
                .iter()
                .filter(|(_, closeout)| closeout.snapshot.starts_with(&snapshot_prefix))
                .map(|(commitment, closeout)| EdgeReceiptCloseoutEvidence {
                    commitment: commitment.clone(),
                    snapshot: closeout.snapshot.clone(),
                    digest: closeout.digest.clone(),
                }),
        );
    }

    Ok(EdgeRetirementInventory {
        version: RETIREMENT_INVENTORY_VERSION,
        project_id: project_id.to_string(),
        relative_paths: paths.into_iter().collect(),
        receipt_bindings,
        receipt_closeouts,
    })
}

pub fn discharge_project_retirement_inventory(
    edges_dir: &Path,
    inventory: &EdgeRetirementInventory,
) -> Result<bool> {
    if inventory.version != RETIREMENT_INVENTORY_VERSION {
        anyhow::bail!("unsupported edge retirement inventory version");
    }
    let expected = inventory
        .relative_paths
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if expected.len() != inventory.relative_paths.len()
        || expected
            .iter()
            .any(|path| !strict_relative_path(Path::new(path)))
    {
        anyhow::bail!("edge retirement inventory contains an invalid path");
    }
    let expected_closeouts = inventory
        .receipt_closeouts
        .iter()
        .map(|closeout| (closeout.commitment.as_str(), closeout))
        .collect::<std::collections::BTreeMap<_, _>>();
    if expected_closeouts.len() != inventory.receipt_closeouts.len() {
        anyhow::bail!("edge retirement inventory contains duplicate receipt closeouts");
    }

    let current = capture_project_retirement_inventory(edges_dir, &inventory.project_id)?;
    if current
        .relative_paths
        .iter()
        .any(|path| !expected.contains(path))
    {
        anyhow::bail!("edge retirement inventory drifted after Prepared");
    }
    if current
        .receipt_bindings
        .iter()
        .any(|(snapshot, digest)| inventory.receipt_bindings.get(snapshot) != Some(digest))
        || current.receipt_closeouts.iter().any(|closeout| {
            expected_closeouts
                .get(closeout.commitment.as_str())
                .is_none_or(|expected| *expected != closeout)
        })
    {
        anyhow::bail!("edge receipt authority drifted after Prepared");
    }

    let index_path = manifest_index_path(edges_dir);
    let mut changed = false;
    if index_path.exists() {
        let mut index = ManifestIndex::load(edges_dir)?;
        if let Some(entry) = index.workspaces.remove(&inventory.project_id) {
            if index
                .retirement_tombstones
                .insert(inventory.project_id.clone(), entry)
                .is_some()
            {
                anyhow::bail!("edge retirement tombstone already conflicts with active workspace");
            }
            index.updated_at = Some(chrono_now_rfc3339());
            index.write_atomic(edges_dir)?;
            changed = true;
        }
    }

    for relative in &inventory.relative_paths {
        let anchored = AnchoredEdgeRoot::open(edges_dir)?;
        changed |= anchored.remove_relative(Path::new(relative))?;
    }

    if index_path.exists() {
        let mut index = ManifestIndex::load(edges_dir)?;
        if index
            .retirement_tombstones
            .remove(&inventory.project_id)
            .is_some()
        {
            index.updated_at = Some(chrono_now_rfc3339());
            index.write_atomic(edges_dir)?;
            changed = true;
        }
        for (snapshot, digest) in &inventory.receipt_bindings {
            match index.receipt_managed_snapshots.get(snapshot) {
                Some(current) if current == digest => {
                    index.receipt_managed_snapshots.remove(snapshot);
                    changed = true;
                }
                Some(_) => anyhow::bail!("edge receipt binding changed during retirement"),
                None => {}
            }
        }
        for closeout in &inventory.receipt_closeouts {
            match index.receipt_closeouts.get(&closeout.commitment) {
                Some(current)
                    if current.snapshot == closeout.snapshot
                        && current.digest == closeout.digest =>
                {
                    index.receipt_closeouts.remove(&closeout.commitment);
                    changed = true;
                }
                Some(_) => anyhow::bail!("edge receipt closeout changed during retirement"),
                None => {}
            }
        }
        let reclamations_before = index.snapshot_reclamations.len();
        index.snapshot_reclamations.retain(|snapshot, _| {
            !snapshot.starts_with(&format!("workspace/{}/snapshots/", inventory.project_id))
        });
        changed |= reclamations_before != index.snapshot_reclamations.len();
        if changed {
            index.updated_at = Some(chrono_now_rfc3339());
            index.write_atomic(edges_dir)?;
        }
    }
    Ok(changed)
}

fn insert_existing_retirement_path(
    anchored: &AnchoredEdgeRoot,
    relative: PathBuf,
    paths: &mut std::collections::BTreeSet<String>,
) -> Result<()> {
    if !strict_relative_path(&relative) {
        anyhow::bail!("edge retirement path is unsafe");
    }
    if !anchored.path_exists(&relative)? {
        return Ok(());
    }
    paths.insert(
        relative
            .to_str()
            .context("edge retirement path is not UTF-8")?
            .to_string(),
    );
    Ok(())
}

fn workspace_entry_paths(
    edges_dir: &Path,
    project_id: &str,
    entry: &WorkspaceIndexEntry,
) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for relative in [
        Some(entry.manifest.as_str()),
        entry.active_snapshot.as_deref(),
        entry.dirty_overlay.as_deref(),
        entry.repo_materialization.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let relative = Path::new(relative);
        if !strict_relative_path(relative) {
            anyhow::bail!("edge workspace path is not a safe relative path");
        }
        paths.push(PathBuf::from("materialized").join(relative));
    }
    let workspace = workspace_manifest_dir(edges_dir, project_id);
    paths.push(
        workspace
            .strip_prefix(edges_dir)
            .context("edge workspace escaped the edge root")?
            .to_path_buf(),
    );
    Ok(paths)
}

#[cfg(unix)]
struct AnchoredEdgeRoot {
    directory: fs::File,
}

#[cfg(unix)]
impl AnchoredEdgeRoot {
    fn open(root: &Path) -> Result<Self> {
        use std::os::unix::fs::OpenOptionsExt;
        let directory = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(root)
            .context("failed to open the anchored edge root")?;
        Ok(Self { directory })
    }

    fn path_exists(&self, relative: &Path) -> Result<bool> {
        let components = strict_path_components(relative)?;
        let Some((name, parents)) = components.split_last() else {
            anyhow::bail!("edge retirement path is empty");
        };
        let parent = match self.open_directory_chain(parents) {
            Ok(parent) => parent,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::CString::new(name.as_bytes())
            .map_err(|_| anyhow::anyhow!("edge path contains NUL"))?;
        let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
        // SAFETY: stat is writable and name is NUL-terminated.
        let status = unsafe {
            libc::fstatat(
                parent.as_raw_fd(),
                name.as_ptr(),
                stat.as_mut_ptr(),
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        if status != 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::NotFound {
                Ok(false)
            } else {
                Err(error).context("edge retirement path failed nofollow validation")
            };
        }
        // SAFETY: fstatat initialized stat on success.
        let stat = unsafe { stat.assume_init() };
        match stat.st_mode & libc::S_IFMT {
            libc::S_IFREG | libc::S_IFDIR => Ok(true),
            libc::S_IFLNK => anyhow::bail!("edge retirement path is symlinked"),
            _ => anyhow::bail!("edge retirement path is not a regular file or directory"),
        }
    }

    fn require_directory(&self, relative: &Path) -> Result<()> {
        let components = strict_path_components(relative)?;
        self.open_directory_chain(&components)?;
        Ok(())
    }

    fn list_directory(&self, relative: &Path) -> Result<Vec<std::ffi::OsString>> {
        let components = strict_path_components(relative)?;
        let directory = self.open_directory_chain(&components)?;
        list_directory_names(&directory)
    }

    fn remove_relative(&self, relative: &Path) -> Result<bool> {
        let components = strict_path_components(relative)?;
        let Some((name, parents)) = components.split_last() else {
            anyhow::bail!("edge retirement path is empty");
        };
        let parent = match self.open_directory_chain(parents) {
            Ok(parent) => parent,
            Err(error)
                if error.chain().any(|cause| {
                    cause
                        .downcast_ref::<std::io::Error>()
                        .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound)
                }) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        remove_entry_at(&parent, name)
    }

    fn open_directory_chain(&self, components: &[std::ffi::OsString]) -> Result<fs::File> {
        let mut current = self.directory.try_clone()?;
        for component in components {
            current = openat_nofollow(current.as_raw_fd(), component, true)
                .with_context(|| format!("edge path component {component:?} is not confined"))?;
        }
        Ok(current)
    }
}

#[cfg(unix)]
fn strict_path_components(path: &Path) -> Result<Vec<std::ffi::OsString>> {
    if !strict_relative_path(path) {
        anyhow::bail!("edge retirement path is unsafe");
    }
    Ok(path
        .components()
        .map(|component| match component {
            Component::Normal(value) => value.to_os_string(),
            _ => unreachable!("strict_relative_path accepted a non-normal component"),
        })
        .collect())
}

#[cfg(unix)]
fn openat_nofollow(
    parent_fd: std::os::fd::RawFd,
    name: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<fs::File> {
    use std::os::fd::FromRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path contains NUL"))?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent_fd is owned by a live File and name is NUL-terminated.
    let fd = unsafe { libc::openat(parent_fd, name.as_ptr(), flags) };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor.
    Ok(unsafe { fs::File::from_raw_fd(fd) })
}

#[cfg(unix)]
fn list_directory_names(directory: &fs::File) -> Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: dup creates an independent descriptor for fdopendir to own.
    let duplicate = unsafe { libc::dup(directory.as_raw_fd()) };
    if duplicate < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // SAFETY: duplicate is a valid directory descriptor.
    let stream = unsafe { libc::fdopendir(duplicate) };
    if stream.is_null() {
        // SAFETY: fdopendir did not take ownership on failure.
        unsafe { libc::close(duplicate) };
        return Err(std::io::Error::last_os_error().into());
    }
    let mut names = Vec::new();
    #[cfg(test)]
    let mut entries_seen = 0_isize;
    loop {
        set_readdir_errno(0);
        // SAFETY: stream remains valid until closed below.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            let errno = readdir_errno();
            if errno == 0 {
                break;
            }
            // SAFETY: stream was returned by fdopendir and is closed once.
            unsafe { libc::closedir(stream) };
            return Err(std::io::Error::from_raw_os_error(errno).into());
        }
        // SAFETY: d_name is NUL-terminated by readdir.
        let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name != b"." && name != b".." {
            names.push(std::ffi::OsString::from_vec(name.to_vec()));
            #[cfg(test)]
            {
                entries_seen += 1;
                if TEST_READDIR_FAIL_AFTER.load(std::sync::atomic::Ordering::SeqCst) == entries_seen
                {
                    // SAFETY: stream was returned by fdopendir and is closed once.
                    unsafe { libc::closedir(stream) };
                    return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
                }
            }
        }
    }
    // SAFETY: stream was returned by fdopendir and is closed exactly once.
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    names.sort();
    Ok(names)
}

#[cfg(test)]
static TEST_READDIR_FAIL_AFTER: std::sync::atomic::AtomicIsize =
    std::sync::atomic::AtomicIsize::new(-1);

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
fn readdir_errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno pointer.
    unsafe { libc::__error() }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn readdir_errno_location() -> *mut libc::c_int {
    // SAFETY: libc returns the calling thread's errno pointer.
    unsafe { libc::__errno_location() }
}

fn set_readdir_errno(value: libc::c_int) {
    // SAFETY: the pointer is the current thread's writable errno slot.
    unsafe { *readdir_errno_location() = value };
}

fn readdir_errno() -> libc::c_int {
    // SAFETY: the pointer is the current thread's readable errno slot.
    unsafe { *readdir_errno_location() }
}

#[cfg(unix)]
fn remove_entry_at(parent: &fs::File, name: &std::ffi::OsStr) -> Result<bool> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let name_c = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| anyhow::anyhow!("edge path contains NUL"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: stat points to writable storage and name_c is NUL-terminated.
    let status = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(false);
        }
        return Err(error.into());
    }
    // SAFETY: fstatat initialized stat on success.
    let stat = unsafe { stat.assume_init() };
    let file_type = stat.st_mode & libc::S_IFMT;
    if file_type == libc::S_IFLNK {
        anyhow::bail!("edge retirement target or intermediate component is symlinked");
    }
    if file_type == libc::S_IFDIR {
        let child = openat_nofollow(parent.as_raw_fd(), name, true)
            .context("edge retirement directory changed during confinement check")?;
        for child_name in list_directory_names(&child)? {
            remove_entry_at(&child, &child_name)?;
        }
        // SAFETY: parent and name identify the already-drained directory.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    } else if file_type == libc::S_IFREG {
        // SAFETY: parent and name identify the regular file checked above.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name_c.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    } else {
        anyhow::bail!("edge retirement target is not a regular file or directory");
    }
    parent.sync_all()?;
    Ok(true)
}

#[cfg(not(unix))]
struct AnchoredEdgeRoot {
    root: PathBuf,
}

#[cfg(not(unix))]
impl AnchoredEdgeRoot {
    fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.to_path_buf(),
        })
    }

    fn validate(&self, relative: &Path) -> Result<PathBuf> {
        if !strict_relative_path(relative) {
            anyhow::bail!("edge retirement path is unsafe");
        }
        let mut current = self.root.clone();
        for component in relative.components() {
            let Component::Normal(component) = component else {
                anyhow::bail!("edge retirement path is unsafe");
            };
            current.push(component);
            if current.exists() && fs::symlink_metadata(&current)?.file_type().is_symlink() {
                anyhow::bail!("edge retirement path is symlinked");
            }
        }
        Ok(current)
    }

    fn path_exists(&self, relative: &Path) -> Result<bool> {
        Ok(self.validate(relative)?.exists())
    }

    fn require_directory(&self, relative: &Path) -> Result<()> {
        if !self.validate(relative)?.is_dir() {
            anyhow::bail!("edge retirement path is not a directory");
        }
        Ok(())
    }

    fn list_directory(&self, relative: &Path) -> Result<Vec<std::ffi::OsString>> {
        let mut names = fs::read_dir(self.validate(relative)?)?
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect::<std::io::Result<Vec<_>>>()?;
        names.sort();
        Ok(names)
    }

    fn remove_relative(&self, relative: &Path) -> Result<bool> {
        let path = self.validate(relative)?;
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error.into()),
            Ok(metadata) if metadata.is_dir() => {
                fs::remove_dir_all(&path)?;
                Ok(true)
            }
            Ok(metadata) if metadata.is_file() => {
                fs::remove_file(&path)?;
                Ok(true)
            }
            Ok(_) => anyhow::bail!("edge retirement target is unsupported"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeMigrationSnapshotLimitsV1 {
    pub max_workspaces: usize,
    pub max_source_file_bytes: u64,
    pub max_total_string_bytes: usize,
}

impl Default for EdgeMigrationSnapshotLimitsV1 {
    fn default() -> Self {
        Self {
            max_workspaces: 1_000_000,
            max_source_file_bytes: 16 * 1024 * 1024,
            max_total_string_bytes: 256 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EdgeMigrationSourceStateV1 {
    Present,
    Missing,
    Corrupt { diagnostic_code: &'static str },
    Unavailable { diagnostic_code: &'static str },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeWorkspaceMigrationRowV1 {
    pub workspace_id: String,
    pub project_id: String,
    pub manifest_source_fingerprint_sha256: String,
    pub repo_id: Option<String>,
    pub head_sha: Option<String>,
    pub active_snapshot_id: Option<String>,
    pub active_dirty_overlay_id: Option<String>,
    pub active_snapshot_path: Option<String>,
    pub dirty_overlay_path: Option<String>,
    pub repo_materialization: Option<String>,
    pub code_source_selector: Option<String>,
    pub code_source_generation: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeMigrationSnapshotV1 {
    pub version: u32,
    pub state: EdgeMigrationSourceStateV1,
    pub schema_version: u32,
    pub schema_fingerprint_sha256: String,
    pub source_fingerprint_sha256: Option<String>,
    pub workspace_count: u64,
    pub active_selector_count: u64,
    pub row_commitment_sha256: String,
    pub workspaces: Vec<EdgeWorkspaceMigrationRowV1>,
}

/// Capture the manifest index and every referenced workspace manifest under
/// the same coordinator used by the owner write paths.
pub fn capture_migration_snapshot_no_create(
    edges_dir: &Path,
    limits: EdgeMigrationSnapshotLimitsV1,
) -> EdgeMigrationSnapshotV1 {
    match with_manifest_coordinator(|| Ok(capture_locked(edges_dir, limits))) {
        Ok(snapshot) => snapshot,
        Err(_) => unavailable_snapshot("edge_manifest_coordinator_unavailable"),
    }
}

fn capture_locked(
    edges_dir: &Path,
    limits: EdgeMigrationSnapshotLimitsV1,
) -> EdgeMigrationSnapshotV1 {
    if !strict_absolute_path(edges_dir) {
        return corrupt_snapshot("edge_manifest_root_path_invalid");
    }
    match fs::symlink_metadata(edges_dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_snapshot(),
        Err(_) => return unavailable_snapshot("edge_manifest_root_metadata_unavailable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_snapshot("edge_manifest_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_snapshot("edge_manifest_root_not_directory");
        }
        Ok(_) => {}
    }
    let materialized = materialized_dir(edges_dir);
    match fs::symlink_metadata(&materialized) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return missing_snapshot(),
        Err(_) => return unavailable_snapshot("edge_materialized_metadata_unavailable"),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return corrupt_snapshot("edge_materialized_root_symlinked");
        }
        Ok(metadata) if !metadata.is_dir() => {
            return corrupt_snapshot("edge_materialized_root_not_directory");
        }
        Ok(_) => {}
    }

    let index_path = manifest_index_path(edges_dir);
    let index_bytes = match read_regular_bounded(&index_path, limits.max_source_file_bytes) {
        Ok(Some(bytes)) => bytes,
        Ok(None) => return missing_snapshot(),
        Err(CaptureReadFailure::Corrupt(code)) => return corrupt_snapshot(code),
        Err(CaptureReadFailure::Unavailable(code)) => return unavailable_snapshot(code),
    };
    let index: ManifestIndex = match serde_json::from_slice::<ManifestIndex>(&index_bytes) {
        Ok(index) if index.version == MANIFEST_VERSION => index,
        _ => return corrupt_snapshot("edge_manifest_index_decode_failed"),
    };
    if index.workspaces.len() > limits.max_workspaces {
        return corrupt_snapshot("edge_manifest_workspace_limit");
    }

    let mut source = Sha256::new();
    source.update(SOURCE_HASH_DOMAIN);
    hash_field(&mut source, &index_bytes);
    let mut workspaces = Vec::with_capacity(index.workspaces.len());
    let mut total_string_bytes = 0usize;
    for (project_id, entry) in index.workspaces {
        if !valid_token(&project_id) {
            return corrupt_snapshot("edge_manifest_project_id_invalid");
        }
        let relative_manifest = Path::new(&entry.manifest);
        if !strict_relative_path(relative_manifest) {
            return corrupt_snapshot("edge_workspace_manifest_path_unsafe");
        }
        match path_has_symlink(&materialized, relative_manifest) {
            Ok(true) => return corrupt_snapshot("edge_workspace_manifest_path_unsafe"),
            Ok(false) => {}
            Err(code) => return unavailable_snapshot(code),
        }
        let manifest_path = materialized.join(relative_manifest);
        let manifest_bytes =
            match read_regular_bounded(&manifest_path, limits.max_source_file_bytes) {
                Ok(Some(bytes)) => bytes,
                Ok(None) => return corrupt_snapshot("edge_workspace_manifest_missing"),
                Err(CaptureReadFailure::Corrupt(code)) => return corrupt_snapshot(code),
                Err(CaptureReadFailure::Unavailable(code)) => return unavailable_snapshot(code),
            };
        let manifest: WorkspaceManifest =
            match serde_json::from_slice::<WorkspaceManifest>(&manifest_bytes) {
                Ok(manifest)
                    if manifest.version == MANIFEST_VERSION
                        && manifest.project_id == project_id =>
                {
                    manifest
                }
                _ => return corrupt_snapshot("edge_workspace_manifest_decode_failed"),
            };
        if !optional_token(&manifest.repo_id)
            || !optional_token(&manifest.active_snapshot_id)
            || !optional_token(&manifest.active_dirty_overlay_id)
            || !optional_relative_path(&entry.active_snapshot)
            || !optional_relative_path(&entry.dirty_overlay)
            || !optional_relative_path(&entry.repo_materialization)
            || !optional_token(&entry.code_source_selector)
            || !optional_token(&entry.code_source_generation)
            || manifest
                .head_sha
                .as_deref()
                .is_some_and(|sha| !valid_commit_sha(sha))
            || entry.active_snapshot.is_some() != manifest.active_snapshot_id.is_some()
            || entry.dirty_overlay.is_some() != manifest.active_dirty_overlay_id.is_some()
            || entry.code_source_selector.is_some() != entry.code_source_generation.is_some()
        {
            return corrupt_snapshot("edge_workspace_selector_invalid");
        }
        total_string_bytes = match total_string_bytes.checked_add(
            project_id.len()
                + entry.manifest.len()
                + optional_len(&manifest.repo_id)
                + optional_len(&manifest.head_sha)
                + optional_len(&manifest.active_snapshot_id)
                + optional_len(&manifest.active_dirty_overlay_id)
                + optional_len(&entry.active_snapshot)
                + optional_len(&entry.dirty_overlay)
                + optional_len(&entry.repo_materialization)
                + optional_len(&entry.code_source_selector)
                + optional_len(&entry.code_source_generation),
        ) {
            Some(value) if value <= limits.max_total_string_bytes => value,
            _ => return corrupt_snapshot("edge_manifest_string_byte_limit"),
        };
        let manifest_fingerprint = domain_hash(
            SOURCE_HASH_DOMAIN,
            [entry.manifest.as_bytes(), manifest_bytes.as_slice()],
        );
        hash_field(&mut source, entry.manifest.as_bytes());
        hash_field(&mut source, &manifest_bytes);
        workspaces.push(EdgeWorkspaceMigrationRowV1 {
            workspace_id: project_id.clone(),
            project_id,
            manifest_source_fingerprint_sha256: manifest_fingerprint,
            repo_id: manifest.repo_id,
            head_sha: manifest.head_sha,
            active_snapshot_id: manifest.active_snapshot_id,
            active_dirty_overlay_id: manifest.active_dirty_overlay_id,
            active_snapshot_path: entry.active_snapshot,
            dirty_overlay_path: entry.dirty_overlay,
            repo_materialization: entry.repo_materialization,
            code_source_selector: entry.code_source_selector,
            code_source_generation: entry.code_source_generation,
        });
    }

    let active_selector_count = workspaces
        .iter()
        .filter(|workspace| workspace.code_source_selector.is_some())
        .count() as u64;
    let row_commitment = hash_rows(&workspaces);
    EdgeMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: EdgeMigrationSourceStateV1::Present,
        schema_version: MANIFEST_VERSION,
        schema_fingerprint_sha256: schema_fingerprint(),
        source_fingerprint_sha256: Some(hex::encode(source.finalize())),
        workspace_count: workspaces.len() as u64,
        active_selector_count,
        row_commitment_sha256: row_commitment,
        workspaces,
    }
}

fn missing_snapshot() -> EdgeMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = EdgeMigrationSourceStateV1::Missing;
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn corrupt_snapshot(code: &'static str) -> EdgeMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = EdgeMigrationSourceStateV1::Corrupt {
        diagnostic_code: code,
    };
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn unavailable_snapshot(code: &'static str) -> EdgeMigrationSnapshotV1 {
    let mut snapshot = empty_snapshot();
    snapshot.state = EdgeMigrationSourceStateV1::Unavailable {
        diagnostic_code: code,
    };
    snapshot.source_fingerprint_sha256 = None;
    snapshot
}

fn empty_snapshot() -> EdgeMigrationSnapshotV1 {
    EdgeMigrationSnapshotV1 {
        version: SNAPSHOT_VERSION_V1,
        state: EdgeMigrationSourceStateV1::Present,
        schema_version: MANIFEST_VERSION,
        schema_fingerprint_sha256: schema_fingerprint(),
        source_fingerprint_sha256: Some(empty_hash(SOURCE_HASH_DOMAIN)),
        workspace_count: 0,
        active_selector_count: 0,
        row_commitment_sha256: empty_hash(ROW_HASH_DOMAIN),
        workspaces: Vec::new(),
    }
}

fn schema_fingerprint() -> String {
    domain_hash(
        SCHEMA_HASH_DOMAIN,
        [
            &MANIFEST_VERSION.to_be_bytes()[..],
            b"workspace+active_snapshot+dirty_overlay+repo_materialization+code_source_selector+code_source_generation",
        ],
    )
}

fn hash_rows(rows: &[EdgeWorkspaceMigrationRowV1]) -> String {
    let mut digest = Sha256::new();
    digest.update(ROW_HASH_DOMAIN);
    for row in rows {
        for value in [
            Some(row.workspace_id.as_str()),
            Some(row.project_id.as_str()),
            Some(row.manifest_source_fingerprint_sha256.as_str()),
            row.repo_id.as_deref(),
            row.head_sha.as_deref(),
            row.active_snapshot_id.as_deref(),
            row.active_dirty_overlay_id.as_deref(),
            row.active_snapshot_path.as_deref(),
            row.dirty_overlay_path.as_deref(),
            row.repo_materialization.as_deref(),
            row.code_source_selector.as_deref(),
            row.code_source_generation.as_deref(),
        ] {
            hash_field(&mut digest, value.unwrap_or("").as_bytes());
        }
    }
    hex::encode(digest.finalize())
}

fn domain_hash<'a>(domain: &[u8], fields: impl IntoIterator<Item = &'a [u8]>) -> String {
    let mut digest = Sha256::new();
    digest.update(domain);
    for field in fields {
        hash_field(&mut digest, field);
    }
    hex::encode(digest.finalize())
}

fn empty_hash(domain: &[u8]) -> String {
    hex::encode(Sha256::new().chain_update(domain).finalize())
}

fn hash_field(digest: &mut Sha256, bytes: &[u8]) {
    digest.update((bytes.len() as u64).to_be_bytes());
    digest.update(bytes);
}

fn strict_absolute_path(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::RootDir | Component::Normal(_)))
}

fn strict_relative_path(path: &Path) -> bool {
    !path.as_os_str().is_empty()
        && !path.is_absolute()
        && !path.to_string_lossy().contains('\\')
        && !path.to_string_lossy().chars().any(char::is_control)
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn path_has_symlink(root: &Path, relative: &Path) -> Result<bool, &'static str> {
    let mut current = root.to_path_buf();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return Ok(true);
        };
        current.push(part);
        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => return Ok(true),
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(_) => return Err("edge_workspace_path_metadata_unavailable"),
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureReadFailure {
    Corrupt(&'static str),
    Unavailable(&'static str),
}

fn read_regular_bounded(
    path: &Path,
    max_bytes: u64,
) -> Result<Option<Vec<u8>>, CaptureReadFailure> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(CaptureReadFailure::Unavailable(
                "edge_manifest_source_metadata_unavailable",
            ));
        }
    };
    if metadata.file_type().is_symlink() {
        return Err(CaptureReadFailure::Corrupt(
            "edge_manifest_source_symlinked",
        ));
    }
    if !metadata.is_file() {
        return Err(CaptureReadFailure::Corrupt(
            "edge_manifest_source_not_regular",
        ));
    }
    if metadata.len() > max_bytes {
        return Err(CaptureReadFailure::Corrupt(
            "edge_manifest_source_byte_limit",
        ));
    }
    let file = fs::File::open(path)
        .map_err(|_| CaptureReadFailure::Unavailable("edge_manifest_source_open_unavailable"))?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| CaptureReadFailure::Unavailable("edge_manifest_source_read_unavailable"))?;
    if bytes.len() as u64 > max_bytes {
        return Err(CaptureReadFailure::Corrupt(
            "edge_manifest_source_byte_limit",
        ));
    }
    Ok(Some(bytes))
}

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 512
        && !value.chars().any(char::is_control)
        && !value.contains(['/', '\\'])
        && !matches!(value, "." | "..")
}

fn optional_token(value: &Option<String>) -> bool {
    value.as_deref().is_none_or(valid_token)
}

fn optional_relative_path(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_none_or(|value| strict_relative_path(Path::new(value)))
}

fn optional_len(value: &Option<String>) -> usize {
    value.as_deref().map(str::len).unwrap_or(0)
}

fn valid_commit_sha(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{ManifestIndex, WorkspaceIndexEntry, WorkspaceManifest};

    #[test]
    fn missing_root_is_typed_and_never_created() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap().join("missing");

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, EdgeMigrationSourceStateV1::Missing);
        assert!(!root.exists());
    }

    #[test]
    fn captures_complete_workspace_and_selector_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manifest = WorkspaceManifest {
            version: MANIFEST_VERSION,
            project_id: "project-a".to_string(),
            repo_id: Some("repo-a".to_string()),
            canonical_path: None,
            git_common_dir: None,
            git_worktree_dir: None,
            branch: None,
            head_sha: Some("1111111111111111111111111111111111111111".to_string()),
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: Some("snapshot-a".to_string()),
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        WorkspaceManifest::write_to(&root, &manifest).unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: Some("snapshot-a".to_string()),
                dirty_overlay: None,
                repo_materialization: Some("repo-a".to_string()),
                code_source_selector: Some("selector-a".to_string()),
                code_source_generation: Some("generation-a".to_string()),
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(&root).unwrap();

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert_eq!(snapshot.state, EdgeMigrationSourceStateV1::Present);
        assert_eq!(snapshot.workspace_count, 1);
        assert_eq!(snapshot.active_selector_count, 1);
        assert_eq!(
            snapshot.workspaces[0].code_source_selector.as_deref(),
            Some("selector-a")
        );
    }

    #[test]
    fn retirement_inventory_covers_legacy_modern_and_materialized_lanes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for relative in [
            "project-a.jsonl",
            "explicit/project-a.jsonl",
            "observed/project-a.jsonl",
            "derived/code/project-a.jsonl",
        ] {
            let path = root.join(relative);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, b"{}\n").unwrap();
        }
        for relative in [
            "materialized/workspace/project-a",
            "materialized/snapshots/snapshot-a",
            "materialized/overlays/overlay-a",
            "materialized/repos/repo-a",
        ] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        fs::write(
            root.join("materialized/workspace/project-a/manifest.json"),
            b"{}",
        )
        .unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: Some("snapshots/snapshot-a".to_string()),
                dirty_overlay: Some("overlays/overlay-a".to_string()),
                repo_materialization: Some("repos/repo-a".to_string()),
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(&root).unwrap();

        let inventory = capture_project_retirement_inventory(&root, "project-a").unwrap();
        assert_eq!(inventory.relative_paths.len(), 9);
        assert!(inventory.relative_paths.contains(&"project-a.jsonl".into()));
        assert!(
            inventory
                .relative_paths
                .contains(&"explicit/project-a.jsonl".into())
        );
        assert!(
            inventory
                .relative_paths
                .contains(&"observed/project-a.jsonl".into())
        );
        assert!(
            inventory
                .relative_paths
                .contains(&"derived/code/project-a.jsonl".into())
        );
        assert!(discharge_project_retirement_inventory(&root, &inventory).unwrap());
        assert!(
            capture_project_retirement_inventory(&root, "project-a")
                .unwrap()
                .relative_paths
                .is_empty()
        );
        assert!(
            !ManifestIndex::load(&root)
                .unwrap()
                .workspaces
                .contains_key("project-a")
        );
    }

    #[test]
    fn retirement_tombstone_resumes_after_publication_crash() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::create_dir_all(root.join("materialized/snapshots/snapshot-a")).unwrap();
        let entry = WorkspaceIndexEntry {
            manifest: "workspace/project-a/manifest.json".to_string(),
            active_snapshot: Some("snapshots/snapshot-a".to_string()),
            dirty_overlay: None,
            repo_materialization: None,
            code_source_selector: None,
            code_source_generation: None,
            git_overlay: None,
            git_overlay_managed: false,
        };
        fs::create_dir_all(root.join("materialized/workspace/project-a")).unwrap();
        fs::write(
            root.join("materialized/workspace/project-a/manifest.json"),
            b"{}",
        )
        .unwrap();
        let mut index = ManifestIndex::new();
        index
            .retirement_tombstones
            .insert("project-a".to_string(), entry);
        index.write_atomic(&root).unwrap();

        let inventory = capture_project_retirement_inventory(&root, "project-a").unwrap();
        assert!(!inventory.relative_paths.is_empty());
        assert!(discharge_project_retirement_inventory(&root, &inventory).unwrap());
        assert!(
            !ManifestIndex::load(&root)
                .unwrap()
                .retirement_tombstones
                .contains_key("project-a")
        );
        assert!(!discharge_project_retirement_inventory(&root, &inventory).unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn retirement_capture_refuses_symlinked_derived_intermediate() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("derived")).unwrap();

        let error = capture_project_retirement_inventory(&root, "project-a").unwrap_err();
        assert!(
            error.to_string().contains("nofollow")
                || error.to_string().contains("confined")
                || error.to_string().contains("symlinked")
        );
    }

    #[cfg(unix)]
    #[test]
    fn retirement_delete_refuses_materialized_intermediate_swap() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join("materialized/snapshots")).unwrap();
        fs::write(root.join("materialized/snapshots/project-a"), b"owned").unwrap();
        let inventory = EdgeRetirementInventory {
            version: RETIREMENT_INVENTORY_VERSION,
            project_id: "project-a".to_string(),
            relative_paths: vec!["materialized/snapshots/project-a".to_string()],
            receipt_bindings: std::collections::BTreeMap::new(),
            receipt_closeouts: Vec::new(),
        };

        fs::remove_file(root.join("materialized/snapshots/project-a")).unwrap();
        fs::remove_dir(root.join("materialized/snapshots")).unwrap();
        fs::write(outside.path().join("project-a"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("materialized/snapshots")).unwrap();

        assert!(discharge_project_retirement_inventory(&root, &inventory).is_err());
        assert_eq!(
            fs::read(outside.path().join("project-a")).unwrap(),
            b"outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retirement_delete_refuses_derived_namespace_swap_after_capture() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.join("derived/code")).unwrap();
        fs::write(root.join("derived/code/project-a.jsonl"), b"{}\n").unwrap();
        let inventory = capture_project_retirement_inventory(&root, "project-a").unwrap();

        fs::remove_file(root.join("derived/code/project-a.jsonl")).unwrap();
        fs::remove_dir(root.join("derived/code")).unwrap();
        fs::write(outside.path().join("project-a.jsonl"), b"outside").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("derived/code")).unwrap();

        assert!(discharge_project_retirement_inventory(&root, &inventory).is_err());
        assert_eq!(
            fs::read(outside.path().join("project-a.jsonl")).unwrap(),
            b"outside"
        );
    }

    #[cfg(unix)]
    #[test]
    fn retirement_capture_refuses_fifo_final_component_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fifo = root.join("project-a.jsonl");
        let fifo_c = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_c is a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(fifo_c.as_ptr(), 0o600) }, 0);
        assert!(capture_project_retirement_inventory(&root, "project-a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retirement_capture_refuses_socket_final_component() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let _listener =
            std::os::unix::net::UnixListener::bind(root.join("project-a.jsonl")).unwrap();
        assert!(capture_project_retirement_inventory(&root, "project-a").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn retirement_capture_refuses_device_final_component_when_supported() {
        use std::os::unix::ffi::OsStrExt;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let device = root.join("project-a.jsonl");
        let device_c = std::ffi::CString::new(device.as_os_str().as_bytes()).unwrap();
        // SAFETY: device_c is a valid NUL-terminated path. Unprivileged hosts
        // may refuse mknod; supported hosts must classify the node without
        // opening it.
        if unsafe { libc::mknod(device_c.as_ptr(), libc::S_IFCHR | 0o600, 0) } == 0 {
            assert!(capture_project_retirement_inventory(&root, "project-a").is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn retirement_capture_propagates_mid_enumeration_readdir_failure() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        for namespace in ["a", "b"] {
            fs::create_dir_all(root.join("derived").join(namespace)).unwrap();
            fs::write(
                root.join("derived").join(namespace).join("project-a.jsonl"),
                b"{}\n",
            )
            .unwrap();
        }
        TEST_READDIR_FAIL_AFTER.store(1, std::sync::atomic::Ordering::SeqCst);
        let result = capture_project_retirement_inventory(&root, "project-a");
        TEST_READDIR_FAIL_AFTER.store(-1, std::sync::atomic::Ordering::SeqCst);
        let error = result.unwrap_err();
        assert!(
            error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|error| error.raw_os_error() == Some(libc::EIO))
        );
    }

    #[test]
    fn missing_referenced_workspace_is_corrupt_not_empty() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(&root).unwrap();

        let snapshot =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());

        assert!(matches!(
            snapshot.state,
            EdgeMigrationSourceStateV1::Corrupt {
                diagnostic_code: "edge_workspace_manifest_missing"
            }
        ));
    }

    #[test]
    fn truncated_manifest_and_source_limit_are_distinct_corrupt_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let manifest = WorkspaceManifest {
            version: MANIFEST_VERSION,
            project_id: "project-a".to_string(),
            repo_id: None,
            canonical_path: None,
            git_common_dir: None,
            git_worktree_dir: None,
            branch: None,
            head_sha: None,
            dirty: false,
            dirty_fingerprint: None,
            active_snapshot_id: None,
            active_dirty_overlay_id: None,
            updated_at: None,
        };
        WorkspaceManifest::write_to(&root, &manifest).unwrap();
        let mut index = ManifestIndex::new();
        index.workspaces.insert(
            "project-a".to_string(),
            WorkspaceIndexEntry {
                manifest: "workspace/project-a/manifest.json".to_string(),
                active_snapshot: None,
                dirty_overlay: None,
                repo_materialization: None,
                code_source_selector: None,
                code_source_generation: None,
                git_overlay: None,
                git_overlay_managed: false,
            },
        );
        index.write_atomic(&root).unwrap();
        let manifest_path = materialized_dir(&root).join("workspace/project-a/manifest.json");
        fs::write(&manifest_path, b"{truncated").unwrap();

        let truncated =
            capture_migration_snapshot_no_create(&root, EdgeMigrationSnapshotLimitsV1::default());
        assert!(matches!(
            truncated.state,
            EdgeMigrationSourceStateV1::Corrupt {
                diagnostic_code: "edge_workspace_manifest_decode_failed"
            }
        ));

        let mut limits = EdgeMigrationSnapshotLimitsV1::default();
        limits.max_source_file_bytes = 1;
        let oversized = capture_migration_snapshot_no_create(&root, limits);
        assert!(matches!(
            oversized.state,
            EdgeMigrationSourceStateV1::Corrupt {
                diagnostic_code: "edge_manifest_source_byte_limit"
            }
        ));
    }

    #[test]
    fn retirement_captures_and_discharges_receipt_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let snapshot = "workspace/project-a/snapshots/snapshot-a".to_string();
        let commitment = format!("project-a:{}:{}", "a".repeat(64), "b".repeat(64));
        let digest = "c".repeat(64);
        let mut index = ManifestIndex::new();
        index.bind_snapshot_receipt(snapshot.clone(), digest.clone());
        index.record_receipt_closeout(commitment.clone(), snapshot.clone(), digest.clone());
        index.write_atomic(&root).unwrap();

        let inventory = capture_project_retirement_inventory(&root, "project-a").unwrap();
        assert_eq!(inventory.receipt_bindings.get(&snapshot), Some(&digest));
        assert_eq!(inventory.receipt_closeouts.len(), 1);
        assert_eq!(inventory.receipt_closeouts[0].commitment, commitment);

        assert!(discharge_project_retirement_inventory(&root, &inventory).unwrap());
        let remaining = capture_project_retirement_inventory(&root, "project-a").unwrap();
        assert!(remaining.relative_paths.is_empty());
        assert!(remaining.receipt_bindings.is_empty());
        assert!(remaining.receipt_closeouts.is_empty());
    }
}

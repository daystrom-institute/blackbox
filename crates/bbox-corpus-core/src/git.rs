use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use crate::json_store::NofollowDirectory;

/// Exact contents of the host-local marker that opts a full independent
/// clone into managed-checkout resolution. The marker opens only the managed
/// gate; callers must still match the clone's durable repo identity to one
/// registered project.
pub const MANAGED_CHECKOUT_MARKER_V1: &str = "blackbox-managed-checkout-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitCommit {
    pub sha: String,
    pub parent_shas: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
}

/// One commit from a complete, exact-HEAD history snapshot captured through
/// `StableGitRepository`. Paths are repository-relative and byte-sorted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableGitHistoryCommit {
    pub oid: String,
    pub parent_oids: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
    pub changed_paths: Vec<String>,
}

/// A full object id verified to name a commit in one exact repository/object
/// environment. The exact worktree root, canonical Git directory, and explicit
/// alternate object directory are captured once so every subsequent tree/blob
/// read uses the same authority inputs without repository rediscovery or a
/// movable ref.
#[derive(Clone)]
pub struct VerifiedCommit {
    repository_root: PathBuf,
    oid: String,
    root_tree_oid: String,
    object_id_hex_len: usize,
    #[cfg(unix)]
    authority: Arc<VerifiedGitAuthority>,
}

impl std::fmt::Debug for VerifiedCommit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedCommit")
            .field("repository_root", &self.repository_root)
            .field("oid", &self.oid)
            .field("root_tree_oid", &self.root_tree_oid)
            .field("object_id_hex_len", &self.object_id_hex_len)
            .finish_non_exhaustive()
    }
}

impl VerifiedCommit {
    pub fn oid(&self) -> &str {
        &self.oid
    }
}

/// A read-only repository lease rooted in held no-follow directory handles.
///
/// Discovery begins from a caller-held worktree directory descriptor. Every
/// later ref and object read uses the captured Git/common/object authorities
/// without reopening the worktree pathname.
#[derive(Clone)]
pub struct StableGitRepository {
    #[cfg(unix)]
    authority: Arc<StableRepositoryAuthority>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableGitNoteSnapshotEntry {
    pub target_oid: String,
    pub bytes: Vec<u8>,
}

/// Immutable contents of one exact notes-ref generation.
///
/// `notes_tip` is resolved once and the tree/blobs are subsequently read by
/// object id, so callers can bind transport metadata and document bytes to
/// the same generation without reopening a moving ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableGitNotesSnapshot {
    pub notes_tip: String,
    pub entries: Vec<StableGitNoteSnapshotEntry>,
}

impl std::fmt::Debug for StableGitRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("StableGitRepository(<held authority>)")
    }
}

impl StableGitRepository {
    pub fn repository_root(&self) -> &Path {
        #[cfg(unix)]
        {
            &self.authority.root
        }
        #[cfg(not(unix))]
        {
            unreachable!("stable Git repositories require Unix")
        }
    }

    pub fn authority_paths(&self) -> Vec<PathBuf> {
        #[cfg(unix)]
        {
            vec![
                self.authority.worktree.path.clone(),
                self.authority.git_dir.path.clone(),
                self.authority.common_dir.path.clone(),
                self.authority.objects.path.clone(),
            ]
        }
        #[cfg(not(unix))]
        {
            Vec::new()
        }
    }

    pub fn common_directory(&self) -> &Path {
        #[cfg(unix)]
        {
            &self.authority.common_dir.path
        }
        #[cfg(not(unix))]
        {
            unreachable!("stable Git repositories require Unix")
        }
    }

    pub fn object_id_hex_len(&self) -> Result<usize> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        read_stable_repository_object_format(&self.authority)
    }

    pub fn is_shallow(&self) -> Result<bool> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            let bytes = run_stable_repository_stdout_bounded_with_timeout(
                &self.authority,
                &["rev-parse", "--is-shallow-repository"],
                "checking exact stable repository depth",
                32,
                GIT_OUTPUT_TIMEOUT,
            )?;
            match std::str::from_utf8(&bytes)
                .context("stable Git shallow probe is not UTF-8")?
                .trim()
            {
                "true" => Ok(true),
                "false" => Ok(false),
                _ => anyhow::bail!("stable Git shallow probe returned an invalid value"),
            }
        }
    }

    /// Capture every commit reachable from one exact verified HEAD.
    ///
    /// This is a complete snapshot, never a cursor delta. The Git child runs
    /// under the stable repository's held-directory authority with ambient
    /// replacements, alternates, lazy fetches, and configuration disabled.
    pub fn complete_history_bounded(
        &self,
        head_oid: &str,
        max_commits: usize,
        max_logical_bytes: usize,
    ) -> Result<Vec<StableGitHistoryCommit>> {
        validate_full_object_id(head_oid)?;
        if max_commits == 0 || max_logical_bytes == 0 {
            anyhow::bail!("stable Git history limits must be nonzero");
        }
        #[cfg(not(unix))]
        {
            let _ = (max_commits, max_logical_bytes);
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            const MARKER: &str = "BBOX_GIT_HISTORY_COMMIT_V1";
            let overhead = max_commits
                .checked_mul(256)
                .context("stable Git history overhead bound overflow")?;
            let output_limit = max_logical_bytes
                .checked_add(overhead)
                .context("stable Git history output bound overflow")?;
            let format =
                format!("--format=format:%x00{MARKER}%x00%H%x00%P%x00%an%x00%ae%x00%B%x00");
            let bytes = run_stable_repository_stdout_bounded_with_timeout(
                &self.authority,
                &[
                    "log",
                    "--topo-order",
                    "--reverse",
                    "--no-renames",
                    "-z",
                    &format,
                    "--name-only",
                    head_oid,
                ],
                "capturing complete exact Git history",
                output_limit,
                GIT_HISTORY_OUTPUT_TIMEOUT,
            )?;
            parse_stable_history_log(&bytes, MARKER, head_oid, max_commits, max_logical_bytes)
        }
    }

    pub fn verified_head(&self) -> Result<Option<VerifiedCommit>> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            let head = self
                .authority
                .git_dir
                .read_regular_bounded("HEAD", 4096, "stable repository HEAD")?
                .context("stable repository HEAD is missing")?;
            let head = std::str::from_utf8(&head)
                .context("stable repository HEAD is not UTF-8")?
                .trim();
            let oid = if let Some(reference) = head.strip_prefix("ref: ") {
                validate_stable_reference(reference)?;
                resolve_stable_repository_ref(&self.authority, reference)?
            } else if head.is_empty() {
                None
            } else {
                Some(head.to_string())
            };
            let Some(oid) = oid else {
                return Ok(None);
            };
            verify_commit_oid_in_stable_unix(duplicate_stable_repository(&self.authority)?, &oid)
                .map(Some)
        }
    }

    pub fn resolve_commit_oid(&self, commitish: &str) -> Result<Option<String>> {
        validate_stable_commitish(commitish)?;
        #[cfg(not(unix))]
        {
            let _ = commitish;
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            resolve_stable_repository_commitish(&self.authority, commitish)
        }
    }

    pub fn verify_commit_oid(&self, oid: &str) -> Result<VerifiedCommit> {
        #[cfg(not(unix))]
        {
            let _ = oid;
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            verify_commit_oid_in_stable_unix(duplicate_stable_repository(&self.authority)?, oid)
        }
    }

    /// Every commit reachable from `from_oid`, or `None` when the history
    /// exceeds `max_commits` and reachability therefore cannot be proved
    /// within bounds. Callers using this as attribution evidence must treat
    /// `None` as UNPROVED, never as error or as proof: raw object-database
    /// existence (`resolve_commit_oid`) is satisfied by unreachable fetched
    /// objects and alternates, so lineage claims need ancestry from a
    /// captured authoritative ref, not addressability.
    pub fn reachable_commit_set(
        &self,
        from_oid: &str,
        max_commits: usize,
    ) -> Result<Option<std::collections::BTreeSet<String>>> {
        validate_full_object_id(from_oid)?;
        #[cfg(not(unix))]
        {
            let _ = max_commits;
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            // One line per commit: 40 hex + newline; one extra commit's worth
            // detects overflow without an unbounded read.
            let bound = max_commits
                .saturating_add(1)
                .saturating_mul(41)
                .min(64 * 1024 * 1024);
            let bytes = run_stable_repository_stdout_bounded(
                &self.authority,
                &["rev-list", from_oid],
                "walking reachable commits",
                bound,
            )?;
            let mut commits = std::collections::BTreeSet::new();
            for line in bytes.split(|byte| *byte == b'\n') {
                if line.is_empty() {
                    continue;
                }
                let Ok(line) = std::str::from_utf8(line) else {
                    return Ok(None);
                };
                if validate_full_object_id(line).is_err() {
                    return Ok(None);
                }
                commits.insert(line.to_string());
                if commits.len() > max_commits {
                    return Ok(None);
                }
            }
            Ok(Some(commits))
        }
    }

    pub fn first_commit_oid(&self, head_oid: &str) -> Result<Option<String>> {
        validate_full_object_id(head_oid)?;
        #[cfg(not(unix))]
        {
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            let bytes = run_stable_repository_stdout_bounded(
                &self.authority,
                &["rev-list", "--max-parents=0", head_oid],
                "deriving stable first commit",
                16 * 1024,
            )?;
            Ok(git_first_commit_from_stdout(&bytes))
        }
    }

    /// Return one commit reachable from any repository ref, when one exists.
    ///
    /// This is the empty-repository proof used by durable identity minting:
    /// an unreadable or unborn HEAD permits a random id only when the captured
    /// repository authority has no commit reachable from any ref.
    pub fn any_commit_oid(&self) -> Result<Option<String>> {
        #[cfg(not(unix))]
        {
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            let bytes = run_stable_repository_stdout_bounded(
                &self.authority,
                &["rev-list", "--max-count=1", "--all"],
                "proving whether stable repository history is empty",
                128,
            )?;
            let oid = std::str::from_utf8(&bytes)
                .context("stable repository history probe is not UTF-8")?
                .trim();
            if oid.is_empty() {
                return Ok(None);
            }
            validate_full_object_id(oid)?;
            Ok(Some(oid.to_string()))
        }
    }

    pub fn snapshot_notes_bounded(
        &self,
        notes_ref: &str,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<Option<Vec<StableGitNoteSnapshotEntry>>> {
        Ok(self
            .snapshot_notes_generation_bounded(notes_ref, max_entries, max_bytes)?
            .map(|snapshot| snapshot.entries))
    }

    pub fn snapshot_notes_generation_bounded(
        &self,
        notes_ref: &str,
        max_entries: usize,
        max_bytes: usize,
    ) -> Result<Option<StableGitNotesSnapshot>> {
        let mut entries = Vec::new();
        let Some(notes_tip) =
            self.visit_notes_generation_bounded(notes_ref, max_entries, max_bytes, |entry| {
                entries.push(entry);
                Ok(())
            })?
        else {
            return Ok(None);
        };
        Ok(Some(StableGitNotesSnapshot { notes_tip, entries }))
    }

    /// Resolve a moving notes ref once, then stream each blob from that exact
    /// immutable tree. The aggregate byte limit bounds work while the caller
    /// controls payload residency (for example, by spooling each document).
    pub fn visit_notes_generation_bounded(
        &self,
        notes_ref: &str,
        max_entries: usize,
        max_bytes: usize,
        mut visit: impl FnMut(StableGitNoteSnapshotEntry) -> Result<()>,
    ) -> Result<Option<String>> {
        validate_stable_reference(notes_ref)?;
        #[cfg(not(unix))]
        {
            let _ = (notes_ref, max_entries, max_bytes, &mut visit);
            anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
        }
        #[cfg(unix)]
        {
            let Some(commit_oid) = resolve_stable_repository_ref(&self.authority, notes_ref)?
            else {
                return Ok(None);
            };
            let commit = verify_commit_oid_in_stable_unix(
                duplicate_stable_repository(&self.authority)?,
                &commit_oid,
            )?;
            let listing = run_stable_repository_stdout_bounded(
                &self.authority,
                &["ls-tree", "-r", "-z", commit.oid()],
                "listing exact stable Git notes tree",
                max_bytes,
            )?;
            let mut listed = Vec::new();
            for raw_entry in listing.split(|byte| *byte == 0) {
                if raw_entry.is_empty() {
                    continue;
                }
                if listed.len() >= max_entries {
                    anyhow::bail!("stable Git notes snapshot exceeds its entry limit");
                }
                let tab = raw_entry
                    .iter()
                    .position(|byte| *byte == b'\t')
                    .context("stable Git notes tree entry is malformed")?;
                let header = std::str::from_utf8(&raw_entry[..tab])
                    .context("stable Git notes tree header is not UTF-8")?;
                let mut fields = header.split_whitespace();
                let (Some(mode), Some(kind), Some(note_oid), None) =
                    (fields.next(), fields.next(), fields.next(), fields.next())
                else {
                    anyhow::bail!("stable Git notes tree header is malformed");
                };
                if mode != "100644" || kind != "blob" {
                    anyhow::bail!("stable Git notes tree contains a non-blob entry");
                }
                validate_full_object_id(note_oid)?;
                if note_oid.len() != commit.oid().len() {
                    anyhow::bail!("stable Git note object id uses the wrong object format");
                }
                let path = std::str::from_utf8(&raw_entry[tab + 1..])
                    .context("stable Git notes tree path is not UTF-8")?;
                let target_oid = path.replace('/', "");
                validate_full_object_id(&target_oid)?;
                if target_oid.len() != commit.oid().len() {
                    anyhow::bail!("stable Git note target uses the wrong object format");
                }
                listed.push((target_oid, note_oid.to_string()));
            }
            listed.sort();
            if listed.windows(2).any(|pair| pair[0].0 == pair[1].0) {
                anyhow::bail!("stable Git notes tree repeats a target object");
            }
            let mut total_bytes = listing.len();
            for (target_oid, note_oid) in listed {
                let remaining = max_bytes
                    .checked_sub(total_bytes)
                    .context("stable Git notes snapshot exceeds its aggregate byte limit")?;
                let bytes = run_stable_repository_stdout_bounded(
                    &self.authority,
                    &["cat-file", "blob", &note_oid],
                    "reading exact stable Git note blob",
                    remaining,
                )?;
                total_bytes = total_bytes
                    .checked_add(bytes.len())
                    .context("stable Git notes snapshot byte count overflow")?;
                visit(StableGitNoteSnapshotEntry { target_oid, bytes })?;
            }
            Ok(Some(commit_oid))
        }
    }
}

#[cfg(unix)]
struct StableDirectory {
    path: PathBuf,
    file: fs::File,
}

#[cfg(unix)]
impl StableDirectory {
    fn ensure_still_current(&self) -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let reopened = open_stable_directory(&self.path, "repository authority revalidation")?;
        let held = self.file.metadata()?;
        let current = reopened.file.metadata()?;
        if held.dev() != current.dev() || held.ino() != current.ino() {
            anyhow::bail!(
                "stable Git directory authority was replaced: {}",
                self.path.display()
            );
        }
        Ok(())
    }

    fn open_directory_optional(&self, name: &str, label: &str) -> Result<Option<Self>> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        if name.is_empty() || name.contains(['/', '\\']) {
            anyhow::bail!("{label} has an invalid relative name");
        }
        let name = CString::new(name).context("stable Git directory name contains a NUL byte")?;
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("opening {label} through held directory authority"));
        }
        Ok(Some(Self {
            path: self.path.join(name.to_string_lossy().as_ref()),
            file: unsafe { fs::File::from_raw_fd(descriptor) },
        }))
    }

    fn open_parent(&self) -> Result<Self> {
        let mut parent_path = self.path.clone();
        if !parent_path.pop() {
            anyhow::bail!("stable Git directory has no parent");
        }
        self.open_directory_optional("..", "stable Git parent directory")?
            .map(|mut parent| {
                parent.path = parent_path;
                parent
            })
            .context("stable Git parent directory is missing")
    }

    fn read_regular_bounded(
        &self,
        name: &str,
        max_bytes: usize,
        label: &str,
    ) -> Result<Option<Vec<u8>>> {
        use std::ffi::CString;
        use std::os::fd::{AsRawFd, FromRawFd};

        if name.is_empty() || name.contains(['/', '\\']) {
            anyhow::bail!("{label} has an invalid relative name");
        }
        let name = CString::new(name).context("stable Git filename contains a NUL byte")?;
        let descriptor = unsafe {
            libc::openat(
                self.file.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if descriptor < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENOENT) {
                return Ok(None);
            }
            return Err(error)
                .with_context(|| format!("opening {label} through held directory authority"));
        }
        let mut file = unsafe { fs::File::from_raw_fd(descriptor) };
        if !file.metadata()?.file_type().is_file() {
            anyhow::bail!("{label} is not a regular file");
        }
        let limit = max_bytes
            .checked_add(1)
            .context("stable Git read byte limit overflow")?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(limit as u64)
            .read_to_end(&mut bytes)?;
        if bytes.len() > max_bytes {
            anyhow::bail!("{label} exceeds its byte limit");
        }
        Ok(Some(bytes))
    }
}

#[cfg(unix)]
#[allow(dead_code)] // Retained handles keep the verified repository authority alive.
struct StableRepositoryAuthority {
    root: PathBuf,
    worktree: StableDirectory,
    git_dir: StableDirectory,
    common_dir: StableDirectory,
    objects: StableDirectory,
}

#[cfg(unix)]
#[allow(dead_code)] // Retained primary/alternate authorities prevent path rebinding.
struct VerifiedGitAuthority {
    primary: StableRepositoryAuthority,
    alternate: Option<StableRepositoryAuthority>,
    sessions: Mutex<VerifiedObjectSessions>,
}

#[cfg(unix)]
struct VerifiedObjectSessions {
    primary: CatFileSession,
    alternate: Option<CatFileSession>,
}

#[cfg(unix)]
struct CatFileSession {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: Option<BufReader<std::process::ChildStdout>>,
    stderr_drain: Option<std::thread::JoinHandle<(Vec<u8>, bool)>>,
    request_timeout: Duration,
    invalid: bool,
}

#[cfg(unix)]
impl Drop for CatFileSession {
    fn drop(&mut self) {
        self.terminate_bounded();
    }
}

#[cfg(unix)]
struct CatObject {
    object_type: String,
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitBlameLine {
    pub commit_sha: String,
    pub author: String,
    pub author_time: Option<String>,
    pub root: PathBuf,
    pub rel_path: String,
}

pub fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let cwd = if path.is_file() {
        path.parent().unwrap_or(path)
    } else {
        path
    };
    let output = git_output(
        cwd,
        &["rev-parse", "--show-toplevel"],
        "deriving repository root",
    )?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    fs::canonicalize(root.trim()).ok()
}

pub fn git_first_commit_for_path(path: &Path) -> Option<String> {
    let output = git_output(
        path,
        &["rev-list", "--max-parents=0", "HEAD"],
        "deriving first commit",
    )?;
    if !output.status.success() {
        return None;
    }
    git_first_commit_from_stdout(&output.stdout)
}

/// Derive the first commit for durable identity minting without collapsing a
/// Git failure into an empty repository.
///
/// `Ok(None)` is returned only after an exact-environment `--all` probe proves
/// that the repository contains no commits. Replacement objects and ambient
/// Git authority are disabled, so a repository cannot choose a different
/// durable family id through `refs/replace` or inherited process state.
pub fn git_first_commit_for_path_strict(path: &Path) -> Result<Option<String>> {
    let directory = NofollowDirectory::open_existing(path)?
        .with_context(|| format!("repository root {} disappeared", path.display()))?;
    let repository = open_stable_git_repository(&directory)?
        .with_context(|| format!("{} is not a stable Git repository", path.display()))?;
    if let Some(head) = repository.verified_head()? {
        return Ok(Some(
            repository
                .first_commit_oid(head.oid())?
                .context("Git reported a commit HEAD but no first commit")?,
        ));
    }

    // An unborn HEAD is the only state that may lawfully mint a random id.
    // Prove the whole captured repository has no commits; if any ref is
    // readable, the absent HEAD is corruption and minting refuses.
    if repository.any_commit_oid()?.is_some() {
        anyhow::bail!(
            "HEAD could not be read even though the repository contains commits; refusing to mint repo_id"
        );
    }
    Ok(None)
}

pub fn git_first_commit_from_stdout(stdout: &[u8]) -> Option<String> {
    let raw = String::from_utf8(stdout.to_vec()).ok()?;
    let mut roots: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.first().map(|line| (*line).to_string())
}

/// True when `path` is inside a SHALLOW clone (one created with
/// `--depth`/`--shallow-since`, or otherwise carrying `.git/shallow`).
///
/// This gates durable `repo_id` minting: `git_first_commit_for_path` runs
/// `git rev-list --max-parents=0 HEAD`, which in a shallow clone returns the
/// grafted shallow BOUNDARY commit, not the repository's true root. Minting a
/// durable identity from that boundary would fabricate a wrong id that then
/// travels in committed `.bbox/config.toml`, so minting must refuse here.
/// Fails CLOSED: a git error or unparseable answer is treated as shallow
/// (`true`), never as a safe-to-mint `false`.
pub fn is_shallow_repository(path: &Path) -> bool {
    let Some(output) = git_output(
        path,
        &["rev-parse", "--is-shallow-repository"],
        "checking shallow repository",
    ) else {
        return true;
    };
    if !output.status.success() {
        return true;
    }
    match String::from_utf8(output.stdout) {
        Ok(s) => s.trim() != "false",
        Err(_) => true,
    }
}

pub fn git_remote_origin_for_path(path: &Path) -> Option<String> {
    let output = git_output(
        path,
        &["config", "remote.origin.url"],
        "deriving remote origin URL",
    )?;
    if !output.status.success() {
        return None;
    }
    let remote = String::from_utf8(output.stdout).ok()?;
    let remote = remote.trim();
    if remote.is_empty() {
        None
    } else {
        Some(remote.to_string())
    }
}

pub fn current_head(root: &Path) -> Option<String> {
    let output = git_output(root, &["rev-parse", "HEAD"], "deriving current HEAD")?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    if sha.is_empty() {
        None
    } else {
        Some(sha.to_string())
    }
}

/// Resolve a ref or SHA to its full commit id, failing closed when the name is
/// missing or does not peel to a commit.
pub fn resolve_commit(root: &Path, r#ref: &str) -> Option<String> {
    let spec = format!("{}^{{commit}}", r#ref);
    let output = git_output(
        root,
        &["rev-parse", "--verify", &spec],
        "resolving commit ref",
    )?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

pub fn current_branch(root: &Path) -> Option<String> {
    let output = git_output(
        root,
        &["rev-parse", "--abbrev-ref", "HEAD"],
        "deriving current branch",
    )?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?;
    let branch = branch.trim().to_string();
    if branch.is_empty() || branch == "HEAD" {
        None
    } else {
        Some(branch)
    }
}

/// Resolve the shared git common directory for `cwd` — the `.git` directory of
/// the repository's main worktree. A linked worktree's common dir points back at
/// the base repo's git dir, so two worktrees of the same repository share one
/// common dir. That shared identity is the basis for resolving a managed fleet
/// worktree (which lives outside the registered repo root) to its registered
/// base project. Returns `None` when `cwd` is not in a git repo or the path can't
/// be canonicalized.
pub fn git_common_dir(cwd: &Path) -> Option<PathBuf> {
    let output = git_output(
        cwd,
        &["rev-parse", "--git-common-dir"],
        "resolving git common dir",
    )?;
    if !output.status.success() {
        return None;
    }
    let raw = String::from_utf8(output.stdout).ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    let path = if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    };
    std::fs::canonicalize(path).ok()
}

/// Return the top of an independent clone carrying the exact managed checkout
/// marker. Linked worktrees use their existing structural/branch gates and do
/// not satisfy this shape because their `.git` entry is a file.
#[allow(clippy::disallowed_methods)]
pub fn managed_checkout_root(path: &Path) -> Option<PathBuf> {
    let root = git_root_for_path(path)?;
    let dot_git = root.join(".git");
    if !dot_git.is_dir() {
        return None;
    }
    let marker = fs::read_to_string(dot_git.join("blackbox-managed-checkout")).ok()?;
    (marker.trim() == MANAGED_CHECKOUT_MARKER_V1).then_some(root)
}

/// Every worktree path of the repository containing `root` — the primary
/// checkout plus every linked worktree — via `git worktree list --porcelain`.
///
/// Used by checkout discovery (design §3.3): a registered repo's linked
/// worktrees are re-findable this way even if the host-local checkout registry
/// is lost. Paths are canonicalized; unparseable/missing paths are skipped.
/// Returns an empty vec when `root` is not in a git repo.
pub fn list_worktree_paths(root: &Path) -> Vec<PathBuf> {
    let Some(output) = git_output(
        root,
        &["worktree", "list", "--porcelain"],
        "listing worktrees",
    ) else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    let Ok(text) = String::from_utf8(output.stdout) else {
        return Vec::new();
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .filter_map(|path| fs::canonicalize(path.trim()).ok())
        .collect()
}

/// Read a file's content from a COMMITTED tree via `git show <ref>:<repo_rel>`,
/// bypassing the working tree entirely (design §4.1: published truth is the
/// committed tree, not the dirty working copy the loader reads today).
///
/// `repo_rel` is the path RELATIVE TO THE REPO ROOT, always `/`-separated (git
/// pathspec form) regardless of host OS. `ref` is any commit-ish (`HEAD`, a
/// branch, a SHA). Returns `None` when the path does not exist at that ref, the
/// ref is unknown, or `root` is not a git repo — never an empty-string
/// false-positive. Bytes are decoded lossily; knowledge/gap entries are UTF-8
/// JSON so this is exact for the intended callers.
pub fn read_committed_file(root: &Path, r#ref: &str, repo_rel: &str) -> Option<String> {
    read_committed_file_bytes(root, r#ref, repo_rel)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Byte-exact counterpart to [`read_committed_file`].
pub fn read_committed_file_bytes(root: &Path, r#ref: &str, repo_rel: &str) -> Option<Vec<u8>> {
    read_committed_file_bytes_with_alternate(root, r#ref, repo_rel, None)
}

pub fn read_committed_file_bytes_with_alternate(
    root: &Path,
    r#ref: &str,
    repo_rel: &str,
    alternate_root: Option<&Path>,
) -> Option<Vec<u8>> {
    let spec = format!("{}:{}", r#ref, repo_rel);
    let output = git_output_with_alternate(
        root,
        &["show", &spec],
        alternate_root,
        "reading committed file",
    )?;
    if !output.status.success() {
        return None;
    }
    Some(output.stdout)
}

/// Verify a full object id as a commit under the hardened exact-read
/// environment used by publication builders.
///
/// Replacement objects, lazy fetching, ambient repository/ref/object
/// redirection, and inherited alternate object directories are disabled.
/// `alternate_root` is the only additional object store honored.
pub fn verify_commit_oid_with_alternate(
    root: &Path,
    oid: &str,
    alternate_root: Option<&Path>,
) -> Result<VerifiedCommit> {
    if !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        anyhow::bail!("exact commit must be a full lowercase hexadecimal object id");
    }
    #[cfg(not(unix))]
    {
        let _ = (root, alternate_root);
        anyhow::bail!("verified committed-tree reads require Unix directory-handle confinement");
    }
    #[cfg(unix)]
    verify_commit_oid_with_alternate_unix(root, oid, alternate_root)
}

/// Resolve one full reference through a held, exact worktree authority.
///
/// The caller must pass the exact worktree root. Repository discovery is
/// confirmed before the ref read, and the read itself uses the captured Git,
/// common-directory, and object-directory handles rather than ambient Git
/// configuration or a later path lookup.
pub fn resolve_stable_reference_oid(root: &Path, reference: &str) -> Result<Option<String>> {
    validate_stable_reference(reference)?;
    #[cfg(not(unix))]
    {
        let _ = root;
        anyhow::bail!("stable Git reference reads require Unix directory-handle confinement");
    }
    #[cfg(unix)]
    {
        let repository = resolve_and_open_stable_repository(root, "stable reference repository")?;
        resolve_stable_repository_ref(&repository, reference)
    }
}

/// Strict bounded blob read for a previously verified exact commit.
///
/// The stdout drain retains at most `max_bytes` while continuing to drain the
/// child into a sink. Oversized output is reported separately from bounded
/// diagnostics.
pub fn read_verified_committed_file_bytes_bounded(
    commit: &VerifiedCommit,
    repo_rel: &str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    read_verified_committed_file_bytes_optional_bounded(commit, repo_rel, max_bytes)?
        .context("committed file is missing from the verified object database")
}

/// Optional counterpart to [`read_verified_committed_file_bytes_bounded`].
///
/// The same verified object authority and byte limits apply, but an absent
/// path is returned distinctly so migration inventory can preserve exact
/// committed-source absence without consulting the working tree.
pub fn read_verified_committed_file_bytes_optional_bounded(
    commit: &VerifiedCommit,
    repo_rel: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>> {
    validate_repository_relative_git_path(repo_rel, "committed file")?;
    #[cfg(not(unix))]
    {
        let _ = (commit, max_bytes);
        anyhow::bail!("verified committed-tree reads require Unix directory-handle confinement");
    }
    #[cfg(unix)]
    {
        let mut sessions = commit
            .authority
            .sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("verified Git object session lock was poisoned"))?;
        let mut raw_entry_count = 0_usize;
        let object = sessions.resolve_path(
            &commit.root_tree_oid,
            repo_rel,
            max_bytes,
            std::time::Instant::now() + GIT_OUTPUT_TIMEOUT,
            &mut raw_entry_count,
        )?;
        let Some(object) = object else {
            return Ok(None);
        };
        if object.object_type != "blob" {
            anyhow::bail!("committed file does not resolve to a blob");
        }
        Ok(Some(object.bytes))
    }
}

struct HardenedWorktreeRoot {
    root: PathBuf,
    git_dir: PathBuf,
    common_dir: PathBuf,
    objects: PathBuf,
}

/// Discover and retain one repository from an already-held no-follow
/// worktree directory. A non-repository is returned as `None`.
pub fn open_stable_git_repository(
    caller_directory: &NofollowDirectory,
) -> Result<Option<StableGitRepository>> {
    #[cfg(not(unix))]
    {
        let _ = caller_directory;
        anyhow::bail!("stable Git repositories require Unix directory-handle confinement");
    }
    #[cfg(unix)]
    {
        caller_directory.ensure_still_current()?;
        let mut worktree = StableDirectory {
            path: caller_directory.path_for_diagnostics().to_path_buf(),
            file: caller_directory.duplicate_descriptor()?,
        };
        let authority = loop {
            match worktree.open_directory_optional(".git", "stable Git directory") {
                Ok(Some(git_dir)) => {
                    let common_dir = duplicate_stable_directory(&git_dir)?;
                    let objects = git_dir
                        .open_directory_optional("objects", "stable Git object directory")?
                        .context("stable Git object directory is missing")?;
                    if configured_alternates_exist(&objects)? {
                        anyhow::bail!(
                            "stable repository reads do not honor repository-configured object alternates"
                        );
                    }
                    break Some(StableRepositoryAuthority {
                        root: worktree.path.clone(),
                        worktree,
                        git_dir,
                        common_dir,
                        objects,
                    });
                }
                Ok(None) => {}
                Err(error) => {
                    return Err(error).context(
                        "stable migration reads do not support linked-worktree .git files",
                    );
                }
            }
            if worktree.path.parent().is_none() || worktree.path == Path::new("/") {
                break None;
            }
            worktree = worktree.open_parent()?;
        };
        caller_directory.ensure_still_current()?;
        Ok(authority.map(|authority| StableGitRepository {
            authority: Arc::new(authority),
        }))
    }
}

fn resolve_hardened_worktree_root(caller_root: &Path, label: &str) -> Result<HardenedWorktreeRoot> {
    let caller_root = caller_root
        .canonicalize()
        .with_context(|| format!("canonicalizing {label} {}", caller_root.display()))?;
    let discovered_root = run_hardened_repository_path_query(
        &caller_root,
        &["rev-parse", "--show-toplevel"],
        "resolving exact worktree root",
    )?;
    if discovered_root != caller_root {
        anyhow::bail!("{label} must be the exact worktree root, not a nested repository path");
    }
    let git_dir = run_hardened_repository_path_query(
        &caller_root,
        &["rev-parse", "--absolute-git-dir"],
        "resolving exact worktree git directory",
    )?;
    let common_dir = run_hardened_repository_path_query(
        &caller_root,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "resolving exact worktree common Git directory",
    )?;
    let objects = common_dir.join("objects").canonicalize().with_context(|| {
        format!(
            "canonicalizing exact worktree object directory under {}",
            common_dir.display()
        )
    })?;
    Ok(HardenedWorktreeRoot {
        root: caller_root,
        git_dir,
        common_dir,
        objects,
    })
}

fn run_hardened_repository_path_query(
    caller_root: &Path,
    args: &[&str],
    action: &'static str,
) -> Result<PathBuf> {
    let mut command = Command::new("git");
    command.arg("-C").arg(caller_root).args(args);
    configure_exact_read_environment(&mut command, None)?;
    let output = run_bounded_with_timeout_and_stdout_limit(
        command,
        caller_root,
        action,
        GIT_OUTPUT_TIMEOUT,
        Some(16 * 1024),
    )
    .with_context(|| format!("running git while {action}"))?;
    ensure_exact_git_success(&output, caller_root, action)?;
    if output.stdout_overflowed {
        anyhow::bail!("{action} output exceeded its byte limit");
    }
    let raw = std::str::from_utf8(&output.stdout)
        .with_context(|| format!("{action} output is not UTF-8"))?
        .trim();
    if raw.is_empty() || raw.contains('\n') || raw.contains('\r') {
        anyhow::bail!("{action} output is malformed");
    }
    PathBuf::from(raw)
        .canonicalize()
        .with_context(|| format!("canonicalizing path returned while {action}: {raw}"))
}

#[cfg(unix)]
fn open_stable_repository(discovered: HardenedWorktreeRoot) -> Result<StableRepositoryAuthority> {
    let worktree = open_stable_directory(&discovered.root, "worktree root")?;
    Ok(StableRepositoryAuthority {
        root: discovered.root,
        worktree,
        git_dir: open_stable_directory(&discovered.git_dir, "Git directory")?,
        common_dir: open_stable_directory(&discovered.common_dir, "common Git directory")?,
        objects: open_stable_directory(&discovered.objects, "Git object directory")?,
    })
}

#[cfg(unix)]
fn resolve_and_open_stable_repository(
    root: &Path,
    label: &str,
) -> Result<StableRepositoryAuthority> {
    let discovered = resolve_hardened_worktree_root(root, label)?;
    let expected = (
        discovered.root.clone(),
        discovered.git_dir.clone(),
        discovered.common_dir.clone(),
        discovered.objects.clone(),
    );
    let authority = open_stable_repository(discovered)?;
    let confirmed = resolve_hardened_worktree_root(root, label)?;
    if expected
        != (
            confirmed.root,
            confirmed.git_dir,
            confirmed.common_dir,
            confirmed.objects,
        )
    {
        anyhow::bail!("{label} changed while its Git authority was being acquired");
    }
    authority.worktree.ensure_still_current()?;
    authority.git_dir.ensure_still_current()?;
    authority.common_dir.ensure_still_current()?;
    authority.objects.ensure_still_current()?;
    Ok(authority)
}

#[cfg(unix)]
fn open_stable_directory(path: &Path, label: &str) -> Result<StableDirectory> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options
        .open(path)
        .with_context(|| format!("opening stable {label} handle {}", path.display()))?;
    Ok(StableDirectory {
        path: path.to_path_buf(),
        file,
    })
}

#[cfg(unix)]
fn configured_alternates_exist(objects: &StableDirectory) -> Result<bool> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd};

    let info_name = CString::new("info").expect("static name has no NUL");
    let info_fd = unsafe {
        libc::openat(
            objects.file.as_raw_fd(),
            info_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if info_fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            return Ok(false);
        }
        return Err(error).with_context(|| {
            format!(
                "opening object info directory through stable handle {}",
                objects.path.display()
            )
        });
    }
    let info = unsafe { fs::File::from_raw_fd(info_fd) };
    let alternates_name = CString::new("alternates").expect("static name has no NUL");
    let alternates_fd = unsafe {
        libc::openat(
            info.as_raw_fd(),
            alternates_name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if alternates_fd >= 0 {
        let _alternates = unsafe { fs::File::from_raw_fd(alternates_fd) };
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        return Ok(false);
    }
    Err(error).with_context(|| {
        format!(
            "checking configured alternates through stable handle {}",
            objects.path.display()
        )
    })
}

#[cfg(unix)]
fn configure_stable_repository_command(
    command: &mut Command,
    repository: &StableRepositoryAuthority,
) -> Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;

    configure_exact_read_environment(command, None)?;
    // macOS exposes directory descriptors under /dev/fd but does not permit
    // path traversal through them. Pin the child cwd to the captured objects
    // inode instead; Git's relative object path then survives directory and
    // checkout renames without reopening an authority pathname.
    command
        .current_dir("/")
        .env("GIT_DIR", "..")
        .env("GIT_COMMON_DIR", "..")
        .env("GIT_OBJECT_DIRECTORY", ".");
    let objects_descriptor = repository.objects.file.as_raw_fd();
    unsafe {
        command.pre_exec(move || {
            if libc::fchdir(objects_descriptor) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(())
}

#[cfg(unix)]
fn read_stable_repository_object_format(repository: &StableRepositoryAuthority) -> Result<usize> {
    let mut command = Command::new("git");
    command.args(["rev-parse", "--show-object-format=storage"]);
    configure_stable_repository_command(&mut command, repository)?;
    let output = run_bounded_with_timeout_and_stdout_limit(
        command,
        &repository.root,
        "reading stable repository object format",
        GIT_OUTPUT_TIMEOUT,
        Some(32),
    )
    .context("running Git to read stable repository object format")?;
    ensure_exact_git_success(
        &output,
        &repository.root,
        "reading stable repository object format",
    )?;
    if output.stdout_overflowed {
        anyhow::bail!("stable repository object format exceeded its byte limit");
    }
    match std::str::from_utf8(&output.stdout)
        .context("stable repository object format is not UTF-8")?
        .trim()
    {
        "sha1" => Ok(40),
        "sha256" => Ok(64),
        other => anyhow::bail!("unsupported repository object format: {other:?}"),
    }
}

fn validate_stable_reference(reference: &str) -> Result<()> {
    if reference.is_empty()
        || reference.len() > 1024
        || !reference.starts_with("refs/")
        || reference
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || reference.contains("..")
        || reference.contains("@{")
        || reference.contains('\\')
        || reference.ends_with('.')
        || reference.ends_with('/')
        || reference.split('/').any(|part| part.is_empty())
    {
        anyhow::bail!("stable Git reference is invalid");
    }
    Ok(())
}

fn validate_full_object_id(oid: &str) -> Result<()> {
    if !matches!(oid.len(), 40 | 64)
        || !oid
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        anyhow::bail!("stable Git object id is invalid");
    }
    Ok(())
}

fn validate_stable_commitish(commitish: &str) -> Result<()> {
    if commitish.is_empty()
        || commitish.len() > 1024
        || commitish.starts_with('-')
        || commitish
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || commitish.contains("@{")
        || commitish.contains('\\')
        || commitish.contains(' ')
    {
        anyhow::bail!("stable Git commit selector is invalid");
    }
    Ok(())
}

#[cfg(unix)]
fn resolve_stable_repository_ref(
    repository: &StableRepositoryAuthority,
    reference: &str,
) -> Result<Option<String>> {
    validate_stable_reference(reference)?;
    let specification = format!("{reference}^{{commit}}");
    let mut command = Command::new("git");
    command.args(["rev-parse", "--verify", &specification]);
    configure_stable_repository_command(&mut command, repository)?;
    let output = run_bounded_with_timeout_and_stdout_limit(
        command,
        &repository.root,
        "resolving stable Git reference",
        GIT_OUTPUT_TIMEOUT,
        Some(128),
    )
    .context("running Git to resolve stable reference")?;
    if !output.status.success() {
        return Ok(None);
    }
    if output.stdout_overflowed {
        anyhow::bail!("stable Git reference output exceeded its byte limit");
    }
    let oid = std::str::from_utf8(&output.stdout)
        .context("stable Git reference output is not UTF-8")?
        .trim();
    validate_full_object_id(oid)?;
    Ok(Some(oid.to_string()))
}

#[cfg(unix)]
fn resolve_stable_repository_commitish(
    repository: &StableRepositoryAuthority,
    commitish: &str,
) -> Result<Option<String>> {
    validate_stable_commitish(commitish)?;
    let specification = format!("{commitish}^{{commit}}");
    let mut command = Command::new("git");
    command.args(["rev-parse", "--verify", "--end-of-options", &specification]);
    configure_stable_repository_command(&mut command, repository)?;
    let output = run_bounded_with_timeout_and_stdout_limit(
        command,
        &repository.root,
        "resolving stable Git commit selector",
        GIT_OUTPUT_TIMEOUT,
        Some(128),
    )
    .context("running Git to resolve stable commit selector")?;
    if !output.status.success() {
        return Ok(None);
    }
    if output.stdout_overflowed {
        anyhow::bail!("stable Git commit selector output exceeded its byte limit");
    }
    let oid = std::str::from_utf8(&output.stdout)
        .context("stable Git commit selector output is not UTF-8")?
        .trim();
    validate_full_object_id(oid)?;
    Ok(Some(oid.to_string()))
}

#[cfg(unix)]
fn run_stable_repository_stdout_bounded(
    repository: &StableRepositoryAuthority,
    args: &[&str],
    action: &'static str,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    run_stable_repository_stdout_bounded_with_timeout(
        repository,
        args,
        action,
        max_bytes,
        GIT_OUTPUT_TIMEOUT,
    )
}

#[cfg(unix)]
fn run_stable_repository_stdout_bounded_with_timeout(
    repository: &StableRepositoryAuthority,
    args: &[&str],
    action: &'static str,
    max_bytes: usize,
    timeout: Duration,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.args(args);
    configure_stable_repository_command(&mut command, repository)?;
    let output = run_bounded_with_timeout_and_stdout_limit(
        command,
        &repository.root,
        action,
        timeout,
        Some(max_bytes),
    )
    .with_context(|| format!("running Git while {action}"))?;
    ensure_exact_git_success(&output, &repository.root, action)?;
    if output.stdout_overflowed {
        anyhow::bail!("{action} output exceeded its byte limit");
    }
    Ok(output.stdout)
}

fn parse_stable_history_log(
    bytes: &[u8],
    marker: &str,
    head_oid: &str,
    max_commits: usize,
    max_logical_bytes: usize,
) -> Result<Vec<StableGitHistoryCommit>> {
    let marker = marker.as_bytes();
    let tokens = bytes.split(|byte| *byte == 0).collect::<Vec<_>>();
    let mut index = 0_usize;
    let mut logical_bytes = 0_usize;
    let mut commits = Vec::new();
    while index < tokens.len() {
        while index < tokens.len() && (tokens[index].is_empty() || tokens[index] == b"\n") {
            index += 1;
        }
        if index == tokens.len() {
            break;
        }
        if tokens[index] != marker {
            anyhow::bail!("stable Git history output has an invalid record boundary");
        }
        index += 1;
        if commits.len() >= max_commits || index.saturating_add(5) > tokens.len() {
            anyhow::bail!("stable Git history exceeds its commit limit or is truncated");
        }
        let oid = history_utf8(tokens[index], "commit object id")?.to_string();
        index += 1;
        let parents = history_utf8(tokens[index], "parent object ids")?;
        index += 1;
        let author_name = history_utf8(tokens[index], "author name")?.to_string();
        index += 1;
        let author_email = history_utf8(tokens[index], "author email")?.to_string();
        index += 1;
        let message = history_utf8(tokens[index], "commit message")?.to_string();
        index += 1;

        validate_full_object_id(&oid)?;
        if oid.len() != head_oid.len() {
            anyhow::bail!("stable Git history mixes object formats");
        }
        let parent_oids = parents
            .split_whitespace()
            .map(|parent| {
                validate_full_object_id(parent)?;
                if parent.len() != head_oid.len() {
                    anyhow::bail!("stable Git history mixes object formats");
                }
                Ok(parent.to_string())
            })
            .collect::<Result<Vec<_>>>()?;

        let mut changed_paths = Vec::new();
        let mut first_path = true;
        while index < tokens.len() && tokens[index] != marker {
            let mut path = tokens[index];
            index += 1;
            if path.is_empty() || path == b"\n" {
                continue;
            }
            if first_path && path.first() == Some(&b'\n') {
                path = &path[1..];
            }
            first_path = false;
            if path.is_empty() {
                continue;
            }
            changed_paths.push(history_utf8(path, "changed path")?.to_string());
        }
        changed_paths.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        changed_paths.dedup();

        logical_bytes = logical_bytes
            .checked_add(oid.len())
            .and_then(|value| {
                parent_oids
                    .iter()
                    .try_fold(value, |value, parent| value.checked_add(parent.len()))
            })
            .and_then(|value| value.checked_add(author_name.len()))
            .and_then(|value| value.checked_add(author_email.len()))
            .and_then(|value| value.checked_add(message.len()))
            .and_then(|value| {
                changed_paths
                    .iter()
                    .try_fold(value, |value, path| value.checked_add(path.len()))
            })
            .context("stable Git history logical byte count overflow")?;
        if logical_bytes > max_logical_bytes {
            anyhow::bail!("stable Git history exceeds its logical byte limit");
        }
        commits.push(StableGitHistoryCommit {
            oid,
            parent_oids,
            author_name,
            author_email,
            message,
            changed_paths,
        });
    }
    if commits.is_empty() || !commits.iter().any(|commit| commit.oid == head_oid) {
        anyhow::bail!("stable Git history does not contain its exact HEAD");
    }
    Ok(commits)
}

fn history_utf8<'a>(bytes: &'a [u8], label: &str) -> Result<&'a str> {
    std::str::from_utf8(bytes).with_context(|| format!("stable Git history {label} is not UTF-8"))
}

#[cfg(unix)]
fn duplicate_stable_directory(directory: &StableDirectory) -> Result<StableDirectory> {
    Ok(StableDirectory {
        path: directory.path.clone(),
        file: directory
            .file
            .try_clone()
            .context("duplicating stable Git directory handle")?,
    })
}

#[cfg(unix)]
fn duplicate_stable_repository(
    repository: &StableRepositoryAuthority,
) -> Result<StableRepositoryAuthority> {
    Ok(StableRepositoryAuthority {
        root: repository.root.clone(),
        worktree: duplicate_stable_directory(&repository.worktree)?,
        git_dir: duplicate_stable_directory(&repository.git_dir)?,
        common_dir: duplicate_stable_directory(&repository.common_dir)?,
        objects: duplicate_stable_directory(&repository.objects)?,
    })
}

#[cfg(unix)]
fn verify_commit_oid_with_alternate_unix(
    root: &Path,
    oid: &str,
    alternate_root: Option<&Path>,
) -> Result<VerifiedCommit> {
    let primary = resolve_and_open_stable_repository(root, "exact-read repository")?;
    let alternate = alternate_root
        .map(|root| resolve_and_open_stable_repository(root, "explicit alternate repository"))
        .transpose()?;
    if alternate.is_none() {
        return verify_commit_oid_in_stable_unix(primary, oid);
    }
    if configured_alternates_exist(&primary.objects)?
        || alternate
            .as_ref()
            .map(|repository| configured_alternates_exist(&repository.objects))
            .transpose()?
            .unwrap_or(false)
    {
        anyhow::bail!(
            "exact publication reads do not honor repository-configured object alternates"
        );
    }

    let object_id_hex_len = read_stable_repository_object_format(&primary)?;
    if oid.len() != object_id_hex_len {
        anyhow::bail!("exact commit object id length does not match repository object format");
    }
    if let Some(alternate) = alternate.as_ref()
        && read_stable_repository_object_format(alternate)? != object_id_hex_len
    {
        anyhow::bail!("explicit alternate repository uses a different object format");
    }

    let mut sessions = VerifiedObjectSessions {
        primary: CatFileSession::spawn(&primary)?,
        alternate: alternate.as_ref().map(CatFileSession::spawn).transpose()?,
    };
    sessions.primary.initialize_alternates(object_id_hex_len)?;
    if let Some(alternate_session) = sessions.alternate.as_mut() {
        alternate_session.initialize_alternates(object_id_hex_len)?;
    }
    if configured_alternates_exist(&primary.objects)? {
        anyhow::bail!("repository-configured object alternates appeared during exact verification");
    }
    if alternate
        .as_ref()
        .map(|repository| configured_alternates_exist(&repository.objects))
        .transpose()?
        .unwrap_or(false)
    {
        anyhow::bail!("repository-configured object alternates appeared during exact verification");
    }
    let commit = sessions
        .read_object(oid, GIT_COMMIT_OBJECT_LIMIT)?
        .context("exact commit object is absent from every verified object database")?;
    if commit.object_type != "commit" {
        anyhow::bail!("exact object id does not name a commit");
    }
    let root_tree_oid =
        parse_commit_tree_oid(&commit.bytes, object_id_hex_len).context("parsing exact commit")?;

    let repository_root = primary.root.clone();
    Ok(VerifiedCommit {
        repository_root,
        oid: oid.to_string(),
        root_tree_oid,
        object_id_hex_len,
        authority: Arc::new(VerifiedGitAuthority {
            primary,
            alternate,
            sessions: Mutex::new(sessions),
        }),
    })
}

#[cfg(unix)]
fn verify_commit_oid_in_stable_unix(
    primary: StableRepositoryAuthority,
    oid: &str,
) -> Result<VerifiedCommit> {
    validate_full_object_id(oid)?;
    if configured_alternates_exist(&primary.objects)? {
        anyhow::bail!(
            "exact publication reads do not honor repository-configured object alternates"
        );
    }
    let object_id_hex_len = read_stable_repository_object_format(&primary)?;
    if oid.len() != object_id_hex_len {
        anyhow::bail!("exact commit object id length does not match repository object format");
    }
    let mut sessions = VerifiedObjectSessions {
        primary: CatFileSession::spawn(&primary)?,
        alternate: None,
    };
    sessions.primary.initialize_alternates(object_id_hex_len)?;
    if configured_alternates_exist(&primary.objects)? {
        anyhow::bail!("repository-configured object alternates appeared during exact verification");
    }
    let commit = sessions
        .read_object(oid, GIT_COMMIT_OBJECT_LIMIT)?
        .context("exact commit object is absent from verified object database")?;
    if commit.object_type != "commit" {
        anyhow::bail!("exact object id does not name a commit");
    }
    let root_tree_oid =
        parse_commit_tree_oid(&commit.bytes, object_id_hex_len).context("parsing exact commit")?;
    let repository_root = primary.root.clone();
    Ok(VerifiedCommit {
        repository_root,
        oid: oid.to_string(),
        root_tree_oid,
        object_id_hex_len,
        authority: Arc::new(VerifiedGitAuthority {
            primary,
            alternate: None,
            sessions: Mutex::new(sessions),
        }),
    })
}

#[cfg(unix)]
const GIT_COMMIT_OBJECT_LIMIT: usize = 16 * 1024 * 1024;

#[cfg(unix)]
const GIT_PATH_TRAVERSAL_LIMIT: usize = 64 * 1024 * 1024;

#[cfg(unix)]
const MAX_VERIFIED_RAW_TREE_ENTRIES: usize = 200_000;

#[cfg(unix)]
fn parse_commit_tree_oid(commit: &[u8], object_id_hex_len: usize) -> Result<String> {
    let first_line = commit
        .split(|byte| *byte == b'\n')
        .next()
        .context("commit object has no tree header")?;
    let tree = first_line
        .strip_prefix(b"tree ")
        .context("commit object does not begin with a tree header")?;
    if tree.len() != object_id_hex_len
        || !tree
            .iter()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        anyhow::bail!("commit tree object id is malformed");
    }
    String::from_utf8(tree.to_vec()).context("commit tree object id is not UTF-8")
}

#[cfg(unix)]
impl VerifiedObjectSessions {
    fn read_object(&mut self, object_id: &str, max_bytes: usize) -> Result<Option<CatObject>> {
        self.read_object_before(
            object_id,
            max_bytes,
            std::time::Instant::now() + GIT_OUTPUT_TIMEOUT,
        )
    }

    fn read_object_before(
        &mut self,
        object_id: &str,
        max_bytes: usize,
        deadline: std::time::Instant,
    ) -> Result<Option<CatObject>> {
        if let Some(object) = self
            .primary
            .read_object_before(object_id, max_bytes, deadline)?
        {
            return Ok(Some(object));
        }
        self.alternate
            .as_mut()
            .map(|session| session.read_object_before(object_id, max_bytes, deadline))
            .transpose()
            .map(Option::flatten)
    }

    fn read_info_before(
        &mut self,
        object_id: &str,
        deadline: std::time::Instant,
    ) -> Result<Option<(String, usize)>> {
        if let Some(info) = self.primary.read_info_before(object_id, deadline)? {
            return Ok(Some(info));
        }
        self.alternate
            .as_mut()
            .map(|session| session.read_info_before(object_id, deadline))
            .transpose()
            .map(Option::flatten)
    }

    fn resolve_path(
        &mut self,
        root_tree_oid: &str,
        repo_rel: &str,
        max_object_bytes: usize,
        deadline: std::time::Instant,
        raw_entry_count: &mut usize,
    ) -> Result<Option<CatObject>> {
        let mut tree_oid = root_tree_oid.to_string();
        let mut traversal_bytes = 0_usize;
        let mut components = repo_rel.split('/').peekable();
        while let Some(component) = components.next() {
            let remaining = GIT_PATH_TRAVERSAL_LIMIT
                .checked_sub(traversal_bytes)
                .context("committed path traversal exceeds its byte limit")?;
            let tree = self
                .read_object_before(&tree_oid, remaining, deadline)?
                .context("committed path references a missing tree")?;
            if tree.object_type != "tree" {
                anyhow::bail!("committed path traversal encountered a non-tree object");
            }
            traversal_bytes = traversal_bytes
                .checked_add(tree.bytes.len())
                .context("committed path traversal byte count overflowed")?;
            prescan_raw_tree_entries(
                &tree.bytes,
                root_tree_oid.len(),
                raw_entry_count,
                MAX_VERIFIED_RAW_TREE_ENTRIES,
            )?;
            let mut matched = None;
            for entry in RawTreeEntryIter::new(&tree.bytes, root_tree_oid.len()) {
                let entry = entry?;
                if entry.name == component.as_bytes() {
                    matched = Some((entry.mode, entry.object_id));
                    break;
                }
            }
            let Some((mode, raw_object_id)) = matched else {
                return Ok(None);
            };
            if components.peek().is_some() {
                if !matches!(mode, b"40000" | b"040000") {
                    anyhow::bail!("committed path traverses through a non-directory entry");
                }
                tree_oid = hex::encode(raw_object_id);
                continue;
            }
            return self.read_object_before(
                &hex::encode(raw_object_id),
                max_object_bytes,
                deadline,
            );
        }
        anyhow::bail!("committed path has no components")
    }
}

#[cfg(unix)]
impl CatFileSession {
    fn spawn(repository: &StableRepositoryAuthority) -> Result<Self> {
        let mut command = Command::new("git");
        command.args(["cat-file", "--batch-command"]);
        configure_stable_repository_command(&mut command, repository)?;
        Self::spawn_command(
            command,
            GIT_OUTPUT_TIMEOUT,
            &format!(
                "spawning stable Git object session for {}",
                repository.root.display()
            ),
        )
    }

    fn spawn_command(
        mut command: Command,
        request_timeout: Duration,
        spawn_context: &str,
    ) -> Result<Self> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().with_context(|| spawn_context.to_string())?;
        let stdin = child
            .stdin
            .take()
            .context("stable Git session has no stdin")?;
        let stdout = child
            .stdout
            .take()
            .context("stable Git session has no stdout")?;
        let mut stderr_pipe = child
            .stderr
            .take()
            .context("stable Git session has no stderr")?;
        let stderr_drain = std::thread::spawn(move || {
            drain_with_retention_limit(&mut stderr_pipe, Some(GIT_STDERR_RETAINED_LIMIT))
        });
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            stderr_drain: Some(stderr_drain),
            request_timeout,
            invalid: false,
        })
    }

    fn read_info(&mut self, spec: &str) -> Result<Option<(String, usize)>> {
        self.read_info_before(spec, std::time::Instant::now() + self.request_timeout)
    }

    fn read_info_before(
        &mut self,
        spec: &str,
        deadline: std::time::Instant,
    ) -> Result<Option<(String, usize)>> {
        self.ensure_valid()?;
        let now = std::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("stable Git object request timed out");
        }
        let deadline = deadline.min(now + self.request_timeout);
        let result = (|| {
            self.send_command("info", spec)?;
            self.read_header(deadline)
        })();
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn initialize_alternates(&mut self, object_id_hex_len: usize) -> Result<()> {
        for salt in 0_u8..=u8::MAX {
            let mut candidate = format!("{salt:02x}").repeat(object_id_hex_len / 2);
            candidate.truncate(object_id_hex_len);
            if candidate.bytes().all(|byte| byte == b'0') {
                continue;
            }
            if self.read_info(&candidate)?.is_none() {
                return Ok(());
            }
        }
        anyhow::bail!("could not find a missing object id to initialize Git alternates")
    }

    fn read_object_before(
        &mut self,
        spec: &str,
        max_bytes: usize,
        deadline: std::time::Instant,
    ) -> Result<Option<CatObject>> {
        self.ensure_valid()?;
        let now = std::time::Instant::now();
        if now >= deadline {
            anyhow::bail!("stable Git object request timed out");
        }
        let deadline = deadline.min(now + self.request_timeout);
        let result = self.read_object_until(spec, max_bytes, deadline);
        if result.is_err() {
            self.invalidate();
        }
        result
    }

    fn read_object_until(
        &mut self,
        spec: &str,
        max_bytes: usize,
        deadline: std::time::Instant,
    ) -> Result<Option<CatObject>> {
        self.send_command("contents", spec)?;
        let Some((object_type, size)) = self.read_header(deadline)? else {
            return Ok(None);
        };
        if size > max_bytes {
            let stdout = self
                .stdout
                .as_mut()
                .context("stable Git object session has no stdout")?;
            discard_exact_until(stdout, size, deadline)
                .context("draining oversized stable Git object")?;
            read_git_object_terminator(stdout, deadline, "oversized")?;
            anyhow::bail!("committed object exceeds its byte limit");
        }
        let mut bytes = vec![0_u8; size];
        let stdout = self
            .stdout
            .as_mut()
            .context("stable Git object session has no stdout")?;
        read_exact_until(stdout, &mut bytes, deadline)
            .context("reading stable Git object bytes")?;
        read_git_object_terminator(stdout, deadline, "")?;
        Ok(Some(CatObject { object_type, bytes }))
    }

    fn send_command(&mut self, command: &str, spec: &str) -> Result<()> {
        if spec.bytes().any(|byte| matches!(byte, b'\n' | b'\r' | 0)) {
            anyhow::bail!("stable Git object request contains a control delimiter");
        }
        let stdin = self
            .stdin
            .as_mut()
            .context("stable Git object session has no stdin")?;
        writeln!(stdin, "{command} {spec}").context("writing stable Git object request")?;
        stdin.flush().context("flushing stable Git object request")
    }

    fn read_header(&mut self, deadline: std::time::Instant) -> Result<Option<(String, usize)>> {
        let stdout = self
            .stdout
            .as_mut()
            .context("stable Git object session has no stdout")?;
        let line = read_bounded_line_until(stdout, 4096, deadline)
            .context("reading stable Git object response header")?;
        if line.ends_with(b" missing\n") {
            return Ok(None);
        }
        let line = line
            .strip_suffix(b"\n")
            .context("stable Git object response header is not terminated")?;
        let text =
            std::str::from_utf8(line).context("stable Git object response header is not UTF-8")?;
        let mut fields = text.split_ascii_whitespace();
        let object_id = fields
            .next()
            .context("stable Git object response has no object id")?;
        let object_type = fields
            .next()
            .context("stable Git object response has no type")?;
        let size = fields
            .next()
            .context("stable Git object response has no size")?
            .parse::<usize>()
            .context("stable Git object response size is invalid")?;
        if fields.next().is_some()
            || !matches!(object_id.len(), 40 | 64)
            || !object_id.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("stable Git object response header is malformed");
        }
        Ok(Some((object_type.to_string(), size)))
    }

    fn ensure_valid(&self) -> Result<()> {
        if self.invalid {
            anyhow::bail!("stable Git object session is invalid");
        }
        Ok(())
    }

    fn invalidate(&mut self) {
        self.terminate_bounded();
    }

    fn terminate_bounded(&mut self) {
        self.invalid = true;
        if self.stdin.is_none() && self.stdout.is_none() && self.stderr_drain.is_none() {
            return;
        }
        self.stdin.take();
        self.stdout.take();
        let _ = self.child.kill();
        poll_child_exit_bounded(GIT_SESSION_SHUTDOWN_TIMEOUT, || {
            matches!(self.child.try_wait(), Ok(Some(_)) | Err(_))
        });
        if self
            .stderr_drain
            .as_ref()
            .is_some_and(|drain| drain.is_finished())
            && let Some(stderr_drain) = self.stderr_drain.take()
        {
            let _ = stderr_drain.join();
        } else {
            self.stderr_drain.take();
        }
    }
}

#[cfg(unix)]
const GIT_SESSION_SHUTDOWN_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(unix)]
fn poll_child_exit_bounded(timeout: Duration, mut exited: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if exited() {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(unix)]
fn read_bounded_line_until(
    reader: &mut BufReader<std::process::ChildStdout>,
    max_bytes: usize,
    deadline: std::time::Instant,
) -> Result<Vec<u8>> {
    let mut line = Vec::new();
    loop {
        wait_for_git_stdout(reader, deadline)?;
        let available = reader
            .fill_buf()
            .context("filling stable Git response buffer")?;
        if available.is_empty() {
            anyhow::bail!("stable Git object session ended unexpectedly");
        }
        let line_end = available.iter().position(|byte| *byte == b'\n');
        let consumed = line_end.map_or(available.len(), |position| position + 1);
        let remaining = max_bytes.saturating_sub(line.len());
        line.extend_from_slice(&available[..consumed.min(remaining)]);
        reader.consume(consumed);
        if consumed > remaining {
            while line_end.is_none() {
                wait_for_git_stdout(reader, deadline)?;
                let available = reader
                    .fill_buf()
                    .context("draining oversized stable Git response header")?;
                if available.is_empty() {
                    break;
                }
                let end = available.iter().position(|byte| *byte == b'\n');
                let consumed = end.map_or(available.len(), |position| position + 1);
                reader.consume(consumed);
                if end.is_some() {
                    break;
                }
            }
            anyhow::bail!("stable Git object response header exceeds its byte limit");
        }
        if line_end.is_some() {
            return Ok(line);
        }
    }
}

#[cfg(unix)]
fn read_exact_until(
    reader: &mut BufReader<std::process::ChildStdout>,
    destination: &mut [u8],
    deadline: std::time::Instant,
) -> Result<()> {
    let mut offset = 0_usize;
    while offset < destination.len() {
        wait_for_git_stdout(reader, deadline)?;
        match reader.read(&mut destination[offset..]) {
            Ok(0) => anyhow::bail!("stable Git object session ended unexpectedly"),
            Ok(read) => offset += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error).context("reading stable Git object response"),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn discard_exact_until(
    reader: &mut BufReader<std::process::ChildStdout>,
    mut remaining: usize,
    deadline: std::time::Instant,
) -> Result<()> {
    let mut buffer = [0_u8; 8192];
    while remaining > 0 {
        let read_len = remaining.min(buffer.len());
        read_exact_until(reader, &mut buffer[..read_len], deadline)?;
        remaining -= read_len;
    }
    Ok(())
}

#[cfg(unix)]
fn read_git_object_terminator(
    reader: &mut BufReader<std::process::ChildStdout>,
    deadline: std::time::Instant,
    qualifier: &str,
) -> Result<()> {
    let mut terminator = [0_u8; 1];
    read_exact_until(reader, &mut terminator, deadline)
        .context("reading stable Git object terminator")?;
    if terminator != [b'\n'] {
        let qualifier = if qualifier.is_empty() {
            String::new()
        } else {
            format!("{qualifier} ")
        };
        anyhow::bail!("{qualifier}stable Git object response has an invalid terminator");
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_git_stdout(
    reader: &BufReader<std::process::ChildStdout>,
    deadline: std::time::Instant,
) -> Result<()> {
    use std::os::fd::AsRawFd;

    if !reader.buffer().is_empty() {
        return Ok(());
    }
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("stable Git object request timed out");
        }
        let timeout_ms = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        let mut descriptor = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, timeout_ms) };
        if result > 0 {
            if descriptor.revents & libc::POLLNVAL != 0 {
                anyhow::bail!("stable Git object response descriptor is invalid");
            }
            return Ok(());
        }
        if result == 0 {
            anyhow::bail!("stable Git object request timed out");
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error).context("waiting for stable Git object response");
        }
    }
}

/// List the files under a directory in a COMMITTED tree via
/// `git ls-tree -r --name-only <ref> -- <dir_rel>`. Returns repo-root-relative,
/// `/`-separated paths (git's native output form), recursively (blobs only).
///
/// `dir_rel` is relative to the repo root, `/`-separated. Used to enumerate a
/// scope's committed `.bbox/knowledge/` entries without touching the working
/// tree. Empty vec when the dir is absent at that ref, the ref is unknown, or
/// `root` is not a git repo.
pub fn list_committed_dir(root: &Path, r#ref: &str, dir_rel: &str) -> Vec<String> {
    list_committed_dir_result(root, r#ref, dir_rel).unwrap_or_default()
}

/// Strict counterpart to [`list_committed_dir`]. A missing directory is a
/// successful empty result, while spawn, timeout, ref, and decoding failures
/// remain distinguishable errors for fail-closed snapshot builders.
pub fn list_committed_dir_result(root: &Path, r#ref: &str, dir_rel: &str) -> Result<Vec<String>> {
    list_committed_dir_result_with_alternate(root, r#ref, dir_rel, None)
}

pub fn list_committed_dir_result_with_alternate(
    root: &Path,
    r#ref: &str,
    dir_rel: &str,
    alternate_root: Option<&Path>,
) -> Result<Vec<String>> {
    let output = git_output_with_alternate(
        root,
        &["ls-tree", "-r", "--name-only", r#ref, "--", dir_rel],
        alternate_root,
        "listing committed dir",
    )
    .with_context(|| format!("listing committed directory in {}", root.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git ls-tree failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let text = String::from_utf8(output.stdout)
        .with_context(|| format!("decoding committed directory in {}", root.display()))?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect())
}

/// Strict bounded committed-tree enumeration for publication builders.
///
/// Reads raw tree objects through the verified session and rejects aggregate
/// overflow, non-regular entries, non-UTF-8 or non-confined paths, duplicates,
/// over-count listings, and over-cap raw entry graphs. Raw entries are
/// pre-scanned as borrowed slices before any per-entry path or object-id
/// allocation. Returned paths are sorted independently of object order.
pub fn list_verified_committed_dir_bounded(
    commit: &VerifiedCommit,
    dir_rel: &str,
    max_entries: usize,
    max_listing_bytes: usize,
) -> Result<Vec<String>> {
    validate_repository_relative_git_path(dir_rel, "committed directory")?;
    #[cfg(not(unix))]
    {
        let _ = (commit, max_entries, max_listing_bytes);
        anyhow::bail!("verified committed-tree reads require Unix directory-handle confinement");
    }
    #[cfg(unix)]
    {
        list_verified_committed_dir_bounded_unix(commit, dir_rel, max_entries, max_listing_bytes)
    }
}

#[cfg(unix)]
struct BorrowedRawTreeEntry<'a> {
    mode: &'a [u8],
    name: &'a [u8],
    object_id: &'a [u8],
}

#[cfg(unix)]
struct RawTreeEntryIter<'a> {
    tree: &'a [u8],
    cursor: usize,
    object_id_bytes: usize,
}

#[cfg(unix)]
struct VerifiedListingBudget {
    max_bytes: usize,
    retained_bytes: usize,
    max_trees: usize,
    tree_count: usize,
    raw_entry_count: usize,
}

#[cfg(unix)]
impl VerifiedListingBudget {
    fn new(max_bytes: usize, max_entries: usize, raw_entry_count: usize) -> Self {
        Self {
            max_bytes,
            retained_bytes: 0,
            max_trees: max_entries
                .saturating_add(1)
                .min(MAX_VERIFIED_RAW_TREE_ENTRIES),
            tree_count: 0,
            raw_entry_count,
        }
    }

    fn charge_bytes(&mut self, bytes: usize) -> Result<()> {
        let retained_bytes = self
            .retained_bytes
            .checked_add(bytes)
            .context("committed directory listing byte count overflowed")?;
        if retained_bytes > self.max_bytes {
            anyhow::bail!("committed directory listing exceeds its byte limit");
        }
        self.retained_bytes = retained_bytes;
        Ok(())
    }

    fn remaining_bytes(&self) -> Result<usize> {
        self.max_bytes
            .checked_sub(self.retained_bytes)
            .context("committed directory listing exceeds its byte limit")
    }

    fn charge_tree(&mut self) -> Result<()> {
        if self.tree_count >= self.max_trees {
            anyhow::bail!("committed directory listing exceeds its tree count limit");
        }
        self.tree_count += 1;
        Ok(())
    }

    fn prescan_tree(&mut self, tree: &[u8], object_id_hex_len: usize) -> Result<()> {
        prescan_raw_tree_entries(
            tree,
            object_id_hex_len,
            &mut self.raw_entry_count,
            MAX_VERIFIED_RAW_TREE_ENTRIES,
        )
    }
}

#[cfg(unix)]
fn list_verified_committed_dir_bounded_unix(
    commit: &VerifiedCommit,
    dir_rel: &str,
    max_entries: usize,
    max_listing_bytes: usize,
) -> Result<Vec<String>> {
    let mut sessions = commit
        .authority
        .sessions
        .lock()
        .map_err(|_| anyhow::anyhow!("verified Git object session lock was poisoned"))?;
    let listing_deadline = std::time::Instant::now() + GIT_OUTPUT_TIMEOUT;
    let mut raw_entry_count = 0_usize;
    let Some(root_tree) = sessions.resolve_path(
        &commit.root_tree_oid,
        dir_rel,
        max_listing_bytes,
        listing_deadline,
        &mut raw_entry_count,
    )?
    else {
        return Ok(Vec::new());
    };
    if root_tree.object_type != "tree" {
        anyhow::bail!("committed directory does not resolve to a tree");
    }

    let mut budget = VerifiedListingBudget::new(max_listing_bytes, max_entries, raw_entry_count);
    budget.charge_tree()?;
    budget.charge_bytes(root_tree.bytes.len())?;
    budget.charge_bytes(
        dir_rel
            .len()
            .checked_add(1)
            .context("committed directory path byte count overflowed")?,
    )?;
    let mut root_path = String::with_capacity(dir_rel.len());
    root_path.push_str(dir_rel);
    let mut files = std::collections::BTreeSet::new();
    let mut trees = vec![(root_path, root_tree.bytes)];
    while let Some((tree_path, tree_bytes)) = trees.pop() {
        budget.prescan_tree(&tree_bytes, commit.object_id_hex_len)?;
        for entry in RawTreeEntryIter::new(&tree_bytes, commit.object_id_hex_len) {
            let entry = entry?;
            let name = std::str::from_utf8(entry.name)
                .context("committed tree contains a non-UTF-8 path component")?;
            let full_path_len = tree_path
                .len()
                .checked_add(1)
                .and_then(|length| length.checked_add(name.len()))
                .context("committed directory path byte count overflowed")?;
            budget.charge_bytes(
                full_path_len
                    .checked_add(1)
                    .context("committed directory path byte count overflowed")?,
            )?;
            let mut full_path = String::with_capacity(full_path_len);
            full_path.push_str(&tree_path);
            full_path.push('/');
            full_path.push_str(name);
            validate_repository_relative_git_path(&full_path, "committed tree path")?;
            match entry.mode {
                b"40000" | b"040000" => {
                    budget.charge_tree()?;
                    let object_id = hex::encode(entry.object_id);
                    let subtree = sessions
                        .read_object_before(
                            &object_id,
                            budget.remaining_bytes()?,
                            listing_deadline,
                        )?
                        .context("committed tree references a missing subtree")?;
                    if subtree.object_type != "tree" {
                        anyhow::bail!("committed tree mode does not reference a tree object");
                    }
                    budget.charge_bytes(subtree.bytes.len())?;
                    trees.push((full_path, subtree.bytes));
                }
                b"100644" | b"100755" => {
                    if files.len() >= max_entries {
                        anyhow::bail!("committed directory listing exceeds its entry limit");
                    }
                    let object_id = hex::encode(entry.object_id);
                    let (object_type, _) = sessions
                        .read_info_before(&object_id, listing_deadline)?
                        .context("committed regular-file entry references a missing object")?;
                    if object_type != "blob" {
                        anyhow::bail!(
                            "committed regular-file mode does not reference a blob object"
                        );
                    }
                    if !files.insert(full_path) {
                        anyhow::bail!("committed directory listing contains a duplicate path");
                    }
                }
                b"120000" | b"160000" => {
                    anyhow::bail!("committed directory listing contains a non-regular-file entry")
                }
                _ => anyhow::bail!("committed tree contains an unexpected entry mode"),
            }
        }
    }
    Ok(files.into_iter().collect())
}

#[cfg(unix)]
impl<'a> RawTreeEntryIter<'a> {
    fn new(tree: &'a [u8], object_id_hex_len: usize) -> Self {
        Self {
            tree,
            cursor: 0,
            object_id_bytes: object_id_hex_len / 2,
        }
    }
}

#[cfg(unix)]
impl<'a> Iterator for RawTreeEntryIter<'a> {
    type Item = Result<BorrowedRawTreeEntry<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.cursor >= self.tree.len() {
            return None;
        }
        let tree = self.tree;
        let cursor = self.cursor;
        let object_id_bytes = self.object_id_bytes;
        let result = (|| {
            let mode_end = tree[cursor..]
                .iter()
                .position(|byte| *byte == b' ')
                .map(|offset| cursor + offset)
                .context("committed tree entry has no mode delimiter")?;
            let name_start = mode_end + 1;
            let name_end = tree[name_start..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|offset| name_start + offset)
                .context("committed tree entry has no name delimiter")?;
            let object_start = name_end + 1;
            let object_end = object_start
                .checked_add(object_id_bytes)
                .context("committed tree object id offset overflowed")?;
            if object_end > tree.len() {
                anyhow::bail!("committed tree entry has a truncated object id");
            }
            let mode = &tree[cursor..mode_end];
            let name = &tree[name_start..name_end];
            if name.is_empty() || name.contains(&b'/') {
                anyhow::bail!("committed tree entry name is malformed");
            }
            Ok((
                BorrowedRawTreeEntry {
                    mode,
                    name,
                    object_id: &tree[object_start..object_end],
                },
                object_end,
            ))
        })();
        match result {
            Ok((entry, object_end)) => {
                self.cursor = object_end;
                Some(Ok(entry))
            }
            Err(error) => {
                self.cursor = self.tree.len();
                Some(Err(error))
            }
        }
    }
}

#[cfg(unix)]
fn prescan_raw_tree_entries(
    tree: &[u8],
    object_id_hex_len: usize,
    raw_entry_count: &mut usize,
    max_raw_entries: usize,
) -> Result<()> {
    let mut names = std::collections::BTreeSet::new();
    for entry in RawTreeEntryIter::new(tree, object_id_hex_len) {
        let entry = entry?;
        if *raw_entry_count >= max_raw_entries {
            anyhow::bail!("committed tree traversal exceeds its raw entry limit");
        }
        *raw_entry_count += 1;
        if !names.insert(entry.name) {
            anyhow::bail!("committed tree contains a duplicate entry name");
        }
    }
    Ok(())
}

#[cfg(test)]
fn parse_bounded_committed_tree_paths(
    listing: &[u8],
    dir_rel: &str,
    max_entries: usize,
    max_listing_bytes: usize,
    object_id_hex_len: usize,
) -> Result<Vec<String>> {
    if listing.len() > max_listing_bytes {
        anyhow::bail!("committed directory listing exceeds its byte limit");
    }
    if listing.is_empty() {
        return Ok(Vec::new());
    }
    if listing.last() != Some(&0) {
        anyhow::bail!("committed directory listing is not NUL terminated");
    }

    let prefix = format!("{dir_rel}/");
    let mut paths = std::collections::BTreeSet::new();
    for raw_record in listing[..listing.len() - 1].split(|byte| *byte == 0) {
        if paths.len() >= max_entries {
            anyhow::bail!("committed directory listing exceeds its entry limit");
        }
        let path_delimiter = raw_record
            .iter()
            .position(|byte| *byte == b'\t')
            .context("committed directory listing entry has no path delimiter")?;
        let metadata = &raw_record[..path_delimiter];
        let raw_path = &raw_record[path_delimiter + 1..];
        let mut fields = metadata.split(|byte| *byte == b' ');
        let mode = fields
            .next()
            .context("committed directory listing entry has no mode")?;
        let object_type = fields
            .next()
            .context("committed directory listing entry has no type")?;
        let object_id = fields
            .next()
            .context("committed directory listing entry has no object id")?;
        if fields.next().is_some()
            || object_id.len() != object_id_hex_len
            || !object_id.iter().all(|byte| byte.is_ascii_hexdigit())
        {
            anyhow::bail!("committed directory listing entry metadata is malformed");
        }
        if object_type != b"blob" || (mode != b"100644" && mode != b"100755") {
            anyhow::bail!("committed directory listing contains a non-regular-file entry");
        }
        let path = std::str::from_utf8(raw_path)
            .context("committed directory listing contains a non-UTF-8 path")?;
        validate_repository_relative_git_path(path, "committed tree path")?;
        if !path.starts_with(&prefix) {
            anyhow::bail!("committed tree path is outside its requested directory");
        }
        if !paths.insert(path.to_string()) {
            anyhow::bail!("committed directory listing contains a duplicate path");
        }
    }
    Ok(paths.into_iter().collect())
}

fn validate_repository_relative_git_path(path: &str, label: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        anyhow::bail!("{label} is not a confined repository-relative path");
    }
    Ok(())
}

/// The best common ancestor of two commit-ishes via `git merge-base <a> <b>`,
/// used to compute a checkout's provisional overlay as a merge-base-relative
/// diff against the published tree (design §4.1). Returns `None` when there is
/// no common ancestor (unrelated histories), a ref is unknown, or `root` is not
/// a git repo.
pub fn merge_base(root: &Path, a: &str, b: &str) -> Option<String> {
    merge_base_with_alternate(root, a, b, None)
}

pub fn merge_base_with_alternate(
    root: &Path,
    a: &str,
    b: &str,
    alternate_root: Option<&Path>,
) -> Option<String> {
    let output = git_output_with_alternate(
        root,
        &["merge-base", a, b],
        alternate_root,
        "computing merge base",
    )?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8(output.stdout).ok()?;
    let sha = sha.trim();
    (!sha.is_empty()).then(|| sha.to_string())
}

/// Map a linked-worktree top to its base repository — the directory whose
/// `.git` *directory* backs the worktree. Structural, no git subprocess: a
/// linked worktree's `.git` marker is a FILE containing
/// `gitdir: <base>/.git/worktrees/<name>`. Returns `None` for primary
/// checkouts (`.git` directory), non-repos, and malformed `.git` files —
/// a plain directory can never satisfy this shape. The returned base is the
/// literal path from the gitdir line, NOT canonicalized; callers comparing
/// against canonical roots must canonicalize it themselves.
// One tiny marker-file read on caller threads that are already doing git
// subprocess I/O (dispatch spawn, write-side worktree resolution) — not a
// tokio worker hot path.
#[allow(clippy::disallowed_methods)]
pub fn linked_worktree_base(worktree_top: &Path) -> Option<PathBuf> {
    let dot_git = worktree_top.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let raw = fs::read_to_string(&dot_git).ok()?;
    let gitdir = raw.strip_prefix("gitdir:")?.trim();
    // <base>/.git/worktrees/<name> → <base>
    let p = Path::new(gitdir);
    let worktrees = p.parent()?; // .../.git/worktrees
    if worktrees.file_name()? != "worktrees" {
        return None;
    }
    let git_dir = worktrees.parent()?; // .../.git
    if git_dir.file_name()? != ".git" {
        return None;
    }
    git_dir.parent().map(|b| b.to_path_buf())
}

pub fn commit_log(root: &Path, since_exclusive: Option<&str>) -> Result<Vec<GitCommit>> {
    let mut args = vec![
        "log".to_string(),
        "--format=%H%x1f%P%x1f%an%x1f%ae%x1f%B%x1e".to_string(),
    ];
    if let Some(since) = since_exclusive.filter(|since| is_ancestor_of_head(root, since)) {
        args.push(format!("{since}..HEAD"));
    }
    let output = git_output_strings(root, &args, "reading commit history")
        .with_context(|| format!("failed to execute git log in {}", root.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git log failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_commit_log(&output.stdout)
}

fn is_ancestor_of_head(root: &Path, since: &str) -> bool {
    let rev_args = vec!["rev-parse".to_string(), format!("{since}^{{commit}}")];
    let Some(rev) = git_output_strings(root, &rev_args, "checking commit existence") else {
        return false;
    };
    if !rev.status.success() {
        tracing::warn!(
            path = %root.display(),
            since,
            "last ingested git commit is not resolvable; forcing full git ingestion"
        );
        return false;
    }

    let merge_base_args = vec![
        "merge-base".to_string(),
        "--is-ancestor".to_string(),
        since.to_string(),
        "HEAD".to_string(),
    ];
    let Some(merge_base) = git_output_strings(root, &merge_base_args, "checking git ancestry")
    else {
        return false;
    };
    if merge_base.status.success() {
        true
    } else {
        tracing::warn!(
            path = %root.display(),
            since,
            "last ingested git commit is not an ancestor of HEAD; forcing full git ingestion"
        );
        false
    }
}

pub fn changed_files_for_commit(root: &Path, sha: &str) -> Result<Vec<String>> {
    let args = [
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "-r",
        sha,
    ];
    let output = git_output(root, &args, "reading changed files")
        .with_context(|| format!("failed to execute git diff-tree in {}", root.display()))?;
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let raw = String::from_utf8(output.stdout)?;
    Ok(raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// Config-resolved git-notes namespace, injected once at daemon startup.
///
/// Dependency inversion: this foundation crate must not reach UP into
/// `blackbox::config` (that would be a workspace cycle). The daemon owns config
/// loading and pushes the resolved value in via [`set_notes_namespace`] right
/// after `config::load()`. Absent injection (standalone use / tests), we fall
/// back to the `BBOX_GIT_NOTES_NAMESPACE` env var, then the `"bbox"` default —
/// the same precedence the inlined `config` lookup used to provide.
static NOTES_NAMESPACE_OVERRIDE: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Install the config-resolved git-notes namespace. Idempotent; first set wins.
/// Called by the daemon at startup so corpus-core need not depend on the root
/// crate's config loader.
pub fn set_notes_namespace(namespace: String) -> Result<()> {
    validate_notes_ref_component(&namespace, "namespace")?;
    let _ = NOTES_NAMESPACE_OVERRIDE.set(namespace);
    Ok(())
}

pub fn notes_namespace() -> String {
    if let Some(ns) = NOTES_NAMESPACE_OVERRIDE.get() {
        if !ns.is_empty() {
            return ns.clone();
        }
    }
    std::env::var("BBOX_GIT_NOTES_NAMESPACE")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| "bbox".to_string())
}

pub const NOTE_DOCUMENT_SEPARATOR: &str = "--bbox-note-separator--";

/// Build a bbox-owned git notes ref for an open-ended note kind.
///
/// Kind `provenance` is used today. `knowledge` is reserved for v2
/// cross-machine knowledge serialization, and future kinds should remain under
/// this namespace instead of adding parallel `refs/notes/bbox-*` roots.
pub fn notes_ref(kind: &str) -> Result<String> {
    let namespace = notes_namespace();
    validate_notes_ref_component(&namespace, "namespace")?;
    validate_notes_ref_component(kind, "kind")?;
    Ok(format!("refs/notes/{namespace}/{kind}"))
}

fn validate_notes_ref_component(value: &str, role: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || matches!(value, "." | "..")
        || value.starts_with('-')
        || value.ends_with('.')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("invalid git notes {role}");
    }
    Ok(())
}

pub fn write_note(root: &Path, notes_ref: &str, commit: &str, body: &str) -> Result<()> {
    let mut command = Command::new("git");
    command.arg("-C").arg(root).args([
        "notes",
        "--ref",
        notes_ref,
        "append",
        &format!("--separator={NOTE_DOCUMENT_SEPARATOR}"),
        "-F",
        "-",
        commit,
    ]);
    let output = run_git_bounded_with_stdin(
        command,
        root,
        "appending git note",
        body.as_bytes().to_vec(),
    )
    .with_context(|| format!("git notes append timed out in {}", root.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git notes append failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Idempotently set `notes.mergeStrategy = union` in the repo at `root`.
///
/// Cross-machine provenance exports push to the same notes ref from multiple
/// machines. Without this setting, `git notes merge` uses the default
/// "manual" strategy which aborts on conflict rather than unioning the note
/// bodies. Setting `union` once per repo makes concurrent provenance pushes
/// safe; git config writes are idempotent so calling this on every export is
/// harmless.
pub fn ensure_notes_merge_strategy_union(root: &Path) -> Result<()> {
    let output = git_output(
        root,
        &["config", "notes.mergeStrategy", "union"],
        "setting notes merge strategy",
    )
    .with_context(|| format!("setting notes.mergeStrategy union in {}", root.display()))?;
    if !output.status.success() {
        anyhow::bail!(
            "git config notes.mergeStrategy union failed in {}: {}",
            root.display(),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

pub fn show_note(root: &Path, notes_ref: &str, commit: &str) -> Result<Option<String>> {
    let Some(output) = git_output(
        root,
        &["notes", "--ref", notes_ref, "show", commit],
        "showing git note",
    ) else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    Ok(Some(String::from_utf8(output.stdout)?))
}

pub fn list_notes(root: &Path, notes_ref: &str) -> Result<Vec<(String, String)>> {
    let Some(output) = git_output(
        root,
        &["notes", "--ref", notes_ref, "list"],
        "listing git notes",
    ) else {
        return Ok(Vec::new());
    };
    if !output.status.success() {
        return Ok(Vec::new());
    }
    let raw = String::from_utf8(output.stdout)?;
    Ok(raw
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let note_sha = parts.next()?;
            let commit_sha = parts.next()?;
            Some((note_sha.to_string(), commit_sha.to_string()))
        })
        .collect())
}

pub fn blame_for_line(file: &Path, line: u64) -> Result<Option<GitBlameLine>> {
    if line == 0 {
        anyhow::bail!("line must be 1-based");
    }
    let file = fs::canonicalize(file)
        .with_context(|| format!("canonicalizing blame path {}", file.display()))?;
    let Some(root) = git_root_for_path(&file) else {
        return Ok(None);
    };
    let rel_path = file
        .strip_prefix(&root)
        .unwrap_or(&file)
        .to_string_lossy()
        .to_string();
    let line_spec = format!("{line},{line}");
    let output = git_output(
        &root,
        &["blame", "--porcelain", "-L", &line_spec, "--", &rel_path],
        "running git blame",
    )
    .with_context(|| format!("failed to execute git blame in {}", root.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    parse_blame_porcelain(&output.stdout, root, rel_path)
}

/// Run blame inside an already-authorized Git root. The relative path is
/// lexical and cannot redirect Git into a different repository through a
/// post-validation symlink swap.
pub fn blame_for_line_in_root(
    root: &Path,
    relative_path: &Path,
    line: u64,
) -> Result<Option<GitBlameLine>> {
    if line == 0 {
        anyhow::bail!("line must be 1-based");
    }
    if relative_path.as_os_str().is_empty()
        || relative_path.is_absolute()
        || relative_path
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        anyhow::bail!("blame path must be a non-empty safe relative path");
    }
    let rel_path = relative_path.to_string_lossy().replace('\\', "/");
    let line_spec = format!("{line},{line}");
    let output = git_output(
        root,
        &["blame", "--porcelain", "-L", &line_spec, "--", &rel_path],
        "running git blame",
    )
    .with_context(|| format!("failed to execute git blame in {}", root.display()))?;
    if !output.status.success() {
        return Ok(None);
    }
    parse_blame_porcelain(&output.stdout, root.to_path_buf(), rel_path)
}

pub fn parse_blame_porcelain(
    stdout: &[u8],
    root: PathBuf,
    rel_path: String,
) -> Result<Option<GitBlameLine>> {
    let raw = String::from_utf8(stdout.to_vec())?;
    let mut lines = raw.lines();
    let Some(header) = lines.next() else {
        return Ok(None);
    };
    let commit_sha = header.split_whitespace().next().unwrap_or("").to_string();
    if commit_sha.is_empty() {
        return Ok(None);
    }
    let mut author = String::new();
    let mut author_time = None;
    for line in lines {
        if let Some(value) = line.strip_prefix("author ") {
            author = value.to_string();
        } else if let Some(value) = line.strip_prefix("author-time ") {
            author_time = value
                .parse::<i64>()
                .ok()
                .and_then(|seconds| chrono::DateTime::from_timestamp(seconds, 0))
                .map(|dt| dt.to_rfc3339());
        }
    }
    Ok(Some(GitBlameLine {
        commit_sha,
        author,
        author_time,
        root,
        rel_path,
    }))
}

pub fn head_fingerprint(root: &Path) -> Option<u64> {
    // HACK: project-file reindex metadata only has `(mtime, size)`, so git
    // history uses `mtime = 0` plus `size = HEAD fingerprint` as a synthetic
    // source-file marker. A cleaner future shape is a parallel GitMeta map
    // keyed by project_id instead of overloading FileMeta.size.
    current_head(root).map(|head| {
        let mut bytes = [0u8; 8];
        for (idx, byte) in head.as_bytes().iter().take(8).enumerate() {
            bytes[idx] = *byte;
        }
        u64::from_be_bytes(bytes)
    })
}

pub fn is_worktree_dirty(root: &Path) -> bool {
    let output = match git_output(
        root,
        &["status", "--porcelain"],
        "checking worktree dirty state",
    ) {
        Some(o) if o.status.success() => o,
        _ => return false,
    };
    !String::from_utf8_lossy(&output.stdout).trim().is_empty()
}

pub fn dirty_fingerprint(root: &Path) -> Option<String> {
    let output = git_output(
        root,
        &["status", "--porcelain", "--no-renames", "-z"],
        "computing dirty fingerprint",
    )?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    if stdout.trim().is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(stdout.as_bytes());
    Some(hex::encode(hasher.finalize()))
}

pub fn parse_commit_log(stdout: &[u8]) -> Result<Vec<GitCommit>> {
    let raw = String::from_utf8(stdout.to_vec())?;
    let mut commits = Vec::new();
    for record in raw.split('\x1e') {
        let record = record.trim_matches('\n');
        if record.trim().is_empty() {
            continue;
        }
        let mut parts = record.splitn(5, '\x1f');
        let Some(sha) = parts.next() else {
            continue;
        };
        let Some(parents) = parts.next() else {
            continue;
        };
        let Some(author_name) = parts.next() else {
            continue;
        };
        let Some(author_email) = parts.next() else {
            continue;
        };
        let Some(message) = parts.next() else {
            continue;
        };
        commits.push(GitCommit {
            sha: sha.trim().to_string(),
            parent_shas: parents
                .split_whitespace()
                .filter(|parent| !parent.is_empty())
                .map(str::to_string)
                .collect(),
            author_name: author_name.trim().to_string(),
            author_email: author_email.trim().to_string(),
            message: message.trim().to_string(),
        });
    }
    Ok(commits)
}

/// Hard ceiling on any git child spawned through this module's output
/// helpers. Session cwds can point into dead automounts (autofs NFS lanes):
/// an unbounded `Command::output()` there polls forever and wedges whatever
/// thread spawned it - observed live 2026-07-11, where
/// `git rev-parse --git-common-dir` against a torn-down NFS lane hung 20+
/// minutes at zero CPU inside the IndexWriterActor's reindex pass, stalling
/// all indexing until the child was killed by hand. 10s is generous for
/// every metadata/log invocation these helpers make against healthy repos.
const GIT_OUTPUT_TIMEOUT: Duration = Duration::from_secs(10);
const GIT_HISTORY_OUTPUT_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const GIT_STDERR_RETAINED_LIMIT: usize = 64 * 1024;
const GIT_STDOUT_RETAINED_LIMIT: usize = 128 * 1024 * 1024;

struct BoundedGitOutput {
    status: std::process::ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    stdout_overflowed: bool,
    stderr_overflowed: bool,
}

impl BoundedGitOutput {
    fn into_output(mut self) -> Output {
        if self.stderr_overflowed {
            append_truncation_marker(
                &mut self.stderr,
                GIT_STDERR_RETAINED_LIMIT,
                b"\n[git stderr truncated]\n",
            );
        }
        Output {
            status: self.status,
            stdout: self.stdout,
            stderr: self.stderr,
        }
    }
}

fn append_truncation_marker(bytes: &mut Vec<u8>, limit: usize, marker: &[u8]) {
    if marker.len() > limit {
        bytes.truncate(limit);
        return;
    }
    bytes.truncate(limit - marker.len());
    bytes.extend_from_slice(marker);
}

fn ensure_exact_git_success(output: &BoundedGitOutput, root: &Path, action: &str) -> Result<()> {
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let suffix = if output.stderr_overflowed {
            " [stderr truncated]"
        } else {
            ""
        };
        anyhow::bail!(
            "{action} failed in {}: {}{suffix}",
            root.display(),
            stderr.trim()
        );
    }
    Ok(())
}

pub fn git_output(path: &Path, args: &[&str], action: &'static str) -> Option<Output> {
    git_output_with_alternate(path, args, None, action)
}

fn git_output_with_alternate(
    path: &Path,
    args: &[&str],
    alternate_root: Option<&Path>,
    action: &'static str,
) -> Option<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args(args);
    configure_alternate_objects(&mut cmd, alternate_root);
    run_git_bounded(cmd, path, action)
}

// Compatibility Git helpers intentionally preserve their historical ambient
// repository and alternate-object behavior. Publication reads use the
// VerifiedCommit path below, which scrubs ambient authority and captures only
// one explicit alternate object directory.
fn configure_alternate_objects(command: &mut Command, alternate_root: Option<&Path>) {
    if let Some(alternate_root) = alternate_root
        && let Some(objects) = git_objects_dir(alternate_root)
    {
        let mut paths = std::env::var_os("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .map(|value| std::env::split_paths(&value).collect::<Vec<_>>())
            .unwrap_or_default();
        if !paths.contains(&objects) {
            paths.push(objects);
        }
        if let Ok(joined) = std::env::join_paths(paths) {
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", joined);
        }
    }
}

fn configure_exact_read_environment(
    command: &mut Command,
    alternate_objects: Option<&Path>,
) -> Result<()> {
    const SCRUBBED_GIT_ENVIRONMENT: &[&str] = &[
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_CONFIG",
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_NOSYSTEM",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_SYSTEM",
        "GIT_DIR",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
        "GIT_GRAFT_FILE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_NAMESPACE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_QUARANTINE_PATH",
        "GIT_REPLACE_REF_BASE",
        "GIT_SHALLOW_FILE",
        "GIT_WORK_TREE",
    ];
    for key in SCRUBBED_GIT_ENVIRONMENT {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        let key_text = key.to_string_lossy();
        if key_text.starts_with("GIT_CONFIG_KEY_") || key_text.starts_with("GIT_CONFIG_VALUE_") {
            command.env_remove(key);
        }
    }
    command
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_OPTIONAL_LOCKS", "0");
    if let Some(objects) = alternate_objects {
        let joined = std::env::join_paths([objects])
            .context("encoding explicit alternate object directory")?;
        command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", joined);
    }
    Ok(())
}

fn git_objects_dir(root: &Path) -> Option<PathBuf> {
    let output = git_output(
        root,
        &["rev-parse", "--git-path", "objects"],
        "resolving git object directory",
    )?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let path = PathBuf::from(value.trim());
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    path.canonicalize().ok()
}

fn git_output_strings(path: &Path, args: &[String], action: &'static str) -> Option<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args(args);
    run_git_bounded(cmd, path, action)
}

/// `Command::output()` with a kill-on-timeout deadline. Stdout/stderr drain
/// on detached threads so a chatty child (git log) can never deadlock the
/// poll loop on a full pipe. On the timeout path the child is killed and
/// the drain threads are deliberately NOT joined: a child stuck in
/// uninterruptible NFS sleep may never be reapable, and a bounded zombie or
/// leaked drain thread is strictly better than re-wedging the caller the
/// timeout exists to protect.
// Blocking spawn/sleep on caller threads that already do git subprocess
// I/O (writer actor passes, resolver memo fills) - never a tokio worker
// hot path; the deadline is the point of this helper.
fn run_git_bounded(cmd: Command, path: &Path, action: &'static str) -> Option<Output> {
    let output = run_bounded_with_timeout_and_stdout_limit(
        cmd,
        path,
        action,
        GIT_OUTPUT_TIMEOUT,
        Some(GIT_STDOUT_RETAINED_LIMIT),
    )?;
    if output.stdout_overflowed {
        tracing::warn!(
            path = %path.display(),
            action,
            limit_bytes = GIT_STDOUT_RETAINED_LIMIT,
            "git stdout exceeded the compatibility helper limit"
        );
        return None;
    }
    Some(output.into_output())
}

fn run_git_bounded_with_stdin(
    cmd: Command,
    path: &Path,
    action: &'static str,
    stdin: Vec<u8>,
) -> Option<Output> {
    run_bounded_with_timeout_stdin_and_stdout_limit(
        cmd,
        path,
        action,
        GIT_OUTPUT_TIMEOUT,
        Some(stdin),
        Some(GIT_STDOUT_RETAINED_LIMIT),
    )
    .and_then(|output| {
        if output.stdout_overflowed {
            tracing::warn!(
                path = %path.display(),
                action,
                limit_bytes = GIT_STDOUT_RETAINED_LIMIT,
                "git stdout exceeded the compatibility helper limit"
            );
            None
        } else {
            Some(output.into_output())
        }
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
fn run_bounded_with_timeout(
    cmd: Command,
    path: &Path,
    action: &'static str,
    timeout: Duration,
) -> Option<Output> {
    run_bounded_with_timeout_and_stdout_limit(cmd, path, action, timeout, None)
        .map(BoundedGitOutput::into_output)
}

#[allow(clippy::disallowed_methods)]
fn run_bounded_with_timeout_and_stdout_limit(
    cmd: Command,
    path: &Path,
    action: &'static str,
    timeout: Duration,
    retained_stdout_limit: Option<usize>,
) -> Option<BoundedGitOutput> {
    run_bounded_with_timeout_stdin_and_stdout_limit(
        cmd,
        path,
        action,
        timeout,
        None,
        retained_stdout_limit,
    )
}

#[allow(clippy::disallowed_methods)]
fn run_bounded_with_timeout_stdin_and_stdout_limit(
    mut cmd: Command,
    path: &Path,
    action: &'static str,
    timeout: Duration,
    stdin_payload: Option<Vec<u8>>,
    retained_stdout_limit: Option<usize>,
) -> Option<BoundedGitOutput> {
    if stdin_payload.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                action,
                "failed to execute git"
            );
            return None;
        }
    };
    let mut stdin_writer = if let Some(payload) = stdin_payload {
        let mut stdin_pipe = child.stdin.take()?;
        Some(std::thread::spawn(move || stdin_pipe.write_all(&payload)))
    } else {
        None
    };
    let mut stdout_pipe = child.stdout.take()?;
    let mut stderr_pipe = child.stderr.take()?;
    let stdout_drain = std::thread::spawn(move || {
        drain_with_retention_limit(&mut stdout_pipe, retained_stdout_limit)
    });
    let stderr_drain = std::thread::spawn(move || {
        drain_with_retention_limit(&mut stderr_pipe, Some(GIT_STDERR_RETAINED_LIMIT))
    });

    let deadline = std::time::Instant::now() + timeout;
    // Escalating poll: most git children here finish in single-digit
    // milliseconds (rev-parse, diff-tree), and a fixed 25ms poll adds ~20ms
    // latency per call - across the tens of thousands of per-commit calls a
    // full reindex pass makes, that compounded to 30+ observed minutes.
    // Start at 1ms and back off toward 25ms for genuinely slow children.
    let mut poll = Duration::from_millis(1);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(writer) = stdin_writer.take() {
                    let _ = writer.join();
                }
                let (stdout, stdout_overflowed) =
                    stdout_drain.join().unwrap_or_else(|_| (Vec::new(), false));
                let (stderr, stderr_overflowed) =
                    stderr_drain.join().unwrap_or_else(|_| (Vec::new(), false));
                return Some(BoundedGitOutput {
                    status,
                    stdout,
                    stderr,
                    stdout_overflowed,
                    stderr_overflowed,
                });
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    let _ = child.kill();
                    // Bounded reap attempt only: a D-state NFS child can be
                    // unkillable, and an indefinite wait() here would
                    // reintroduce the hang. try_wait polls for up to 2s,
                    // then the zombie (if any) is abandoned.
                    let reap_deadline = std::time::Instant::now() + Duration::from_secs(2);
                    while std::time::Instant::now() < reap_deadline {
                        if matches!(child.try_wait(), Ok(Some(_)) | Err(_)) {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                    tracing::warn!(
                        path = %path.display(),
                        action,
                        timeout_secs = timeout.as_secs(),
                        "git timed out and was killed (dead mount or hung child); treating as failure"
                    );
                    return None;
                }
                std::thread::sleep(poll);
                poll = (poll * 2).min(Duration::from_millis(25));
            }
            Err(err) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    action,
                    "failed to wait on git"
                );
                return None;
            }
        }
    }
}

fn drain_with_retention_limit(
    pipe: &mut impl std::io::Read,
    retained_limit: Option<usize>,
) -> (Vec<u8>, bool) {
    use std::io::Read;

    let mut retained_bytes = Vec::new();
    let Some(limit) = retained_limit else {
        let _ = pipe.read_to_end(&mut retained_bytes);
        return (retained_bytes, false);
    };

    let mut retained = pipe.by_ref().take(limit as u64);
    let _ = retained.read_to_end(&mut retained_bytes);
    drop(retained);
    let mut overflow_probe = [0_u8; 1];
    let overflowed = pipe.read(&mut overflow_probe).unwrap_or(0) != 0;
    let _ = std::io::copy(pipe, &mut std::io::sink());
    (retained_bytes, overflowed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn stable_history_protocol_preserves_graph_metadata_and_paths() {
        let root = "1".repeat(40);
        let head = "2".repeat(40);
        let marker = "BBOX_GIT_HISTORY_COMMIT_V1";
        let bytes = format!(
            "\0{marker}\0{root}\0\0A\0a@example.invalid\0root\n\0\nREADME.md\0\0\0{marker}\0{head}\0{root}\0B\0b@example.invalid\0head\n\0\nsrc/lib.rs\0src/main.rs\0\n"
        );
        let commits = parse_stable_history_log(bytes.as_bytes(), marker, &head, 2, 4096).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].changed_paths, vec!["README.md"]);
        assert_eq!(commits[1].parent_oids, vec![root]);
        assert_eq!(commits[1].changed_paths, vec!["src/lib.rs", "src/main.rs"]);
    }

    #[test]
    fn run_bounded_kills_hung_child_at_the_deadline() {
        // gap context: session cwds can point into dead NFS automounts,
        // where a spawned git polls forever and wedges the writer actor.
        // The bounded runner must kill and return None instead of hanging.
        let started = std::time::Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let out = run_bounded_with_timeout(
            cmd,
            Path::new("/tmp"),
            "test-hang",
            Duration::from_millis(300),
        );
        assert!(out.is_none(), "hung child must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must return promptly after the deadline, not wait for the child"
        );
    }

    #[test]
    fn run_bounded_with_stdin_kills_hung_child_at_the_deadline() {
        let started = std::time::Instant::now();
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let out = run_bounded_with_timeout_stdin_and_stdout_limit(
            cmd,
            Path::new("/tmp"),
            "test-hang-with-stdin",
            Duration::from_millis(300),
            Some(vec![b'x'; 1024 * 1024]),
            None,
        );
        assert!(out.is_none(), "hung child with stdin must yield None");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must return promptly after the deadline, not wait for the child"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_git_session_kills_and_invalidates_a_stalled_request() {
        let mut command = Command::new("sh");
        command.args(["-c", "read line; exec sleep 30"]);
        let mut session = CatFileSession::spawn_command(
            command,
            Duration::from_millis(100),
            "spawning stalled test session",
        )
        .unwrap();
        let started = std::time::Instant::now();
        let error = session
            .read_info("1111111111111111111111111111111111111111")
            .unwrap_err();
        assert!(format!("{error:#}").contains("timed out"));
        assert!(session.invalid);
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(
            session
                .read_info("1111111111111111111111111111111111111111")
                .unwrap_err()
                .to_string()
                .contains("invalid")
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_git_session_drop_does_not_join_an_unclosed_stderr_pipe() {
        let mut command = Command::new("sh");
        command.args(["-c", "sleep 2 >&2 &"]);
        let session = CatFileSession::spawn_command(
            command,
            Duration::from_secs(1),
            "spawning unclosed-stderr test session",
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(50));

        let started = std::time::Instant::now();
        drop(session);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "session destruction must detach a drain held open by another process"
        );
    }

    #[cfg(unix)]
    #[test]
    fn bounded_child_reap_abandons_injected_unreapable_child() {
        let started = std::time::Instant::now();
        let exited = poll_child_exit_bounded(Duration::from_millis(25), || false);
        assert!(!exited);
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn run_bounded_returns_full_output_for_fast_child() {
        let mut cmd = Command::new("echo");
        cmd.arg("bounded-ok");
        let out =
            run_bounded_with_timeout(cmd, Path::new("/tmp"), "test-fast", Duration::from_secs(10))
                .expect("fast child must succeed");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "bounded-ok");
    }

    #[test]
    fn bounded_runner_caps_stderr_and_preserves_overflow_diagnostic() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "head -c 70000 /dev/zero >&2; exit 7"]);
        let output = run_bounded_with_timeout_and_stdout_limit(
            cmd,
            Path::new("/tmp"),
            "test-stderr-bound",
            Duration::from_secs(10),
            Some(16),
        )
        .unwrap();
        assert_eq!(output.stderr.len(), GIT_STDERR_RETAINED_LIMIT);
        assert!(output.stderr_overflowed);
        let process_output = output.into_output();
        assert_eq!(process_output.stderr.len(), GIT_STDERR_RETAINED_LIMIT);
        assert!(
            process_output
                .stderr
                .ends_with(b"\n[git stderr truncated]\n")
        );
    }

    #[test]
    fn exact_read_environment_scrubs_redirection_and_disables_lazy_fetch() {
        let alternate = tempfile::tempdir().unwrap();
        let mut command = Command::new("git");
        command
            .env("GIT_DIR", "/ambient/repository")
            .env("GIT_ALTERNATE_OBJECT_DIRECTORIES", "/ambient/objects");
        configure_exact_read_environment(&mut command, Some(alternate.path())).unwrap();
        let environment = command
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_string_lossy().into_owned(),
                    value.map(|value| value.to_string_lossy().into_owned()),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();

        assert_eq!(environment.get("GIT_DIR"), Some(&None));
        assert_eq!(
            environment.get("GIT_NO_LAZY_FETCH"),
            Some(&Some("1".to_string()))
        );
        assert_eq!(
            environment.get("GIT_NO_REPLACE_OBJECTS"),
            Some(&Some("1".to_string()))
        );
        assert_ne!(
            environment.get("GIT_ALTERNATE_OBJECT_DIRECTORIES"),
            Some(&Some("/ambient/objects".to_string()))
        );
    }

    #[test]
    fn parse_commit_log_handles_messages_and_parents() {
        let raw = b"abc\x1fp1 p2\x1fAlice\x1fa@example.test\x1fsubject\n\nbody\x1e";
        let commits = parse_commit_log(raw).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].sha, "abc");
        assert_eq!(commits[0].parent_shas, vec!["p1", "p2"]);
        assert_eq!(commits[0].author_name, "Alice");
        assert_eq!(commits[0].author_email, "a@example.test");
        assert_eq!(commits[0].message, "subject\n\nbody");
    }

    #[test]
    fn parse_blame_porcelain_extracts_commit_author_and_time() {
        let raw = b"abc123 1 1 1\nauthor Ada Lovelace\nauthor-mail <ada@example.test>\nauthor-time 1700000000\n\tlet x = 1;\n";
        let blame = parse_blame_porcelain(raw, PathBuf::from("/repo"), "src/main.rs".into())
            .unwrap()
            .unwrap();

        assert_eq!(blame.commit_sha, "abc123");
        assert_eq!(blame.author, "Ada Lovelace");
        assert_eq!(blame.rel_path, "src/main.rs");
        assert_eq!(
            blame.author_time.as_deref(),
            Some("2023-11-14T22:13:20+00:00")
        );
    }

    #[test]
    fn commit_log_propagates_git_failure() {
        let not_a_repo = tempfile::tempdir().unwrap();
        let error = commit_log(not_a_repo.path(), None).unwrap_err();
        assert!(error.to_string().contains("git log failed"));
    }

    #[test]
    fn commit_log_falls_back_to_full_when_since_is_not_ancestor() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        run_git(repo.path(), &["config", "user.email", "test@example.test"]);
        std::fs::write(repo.path().join("README.md"), "one\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "old root"]);
        let old_head = current_head(repo.path()).unwrap();

        run_git(repo.path(), &["checkout", "--orphan", "rewritten"]);
        std::fs::write(repo.path().join("README.md"), "two\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "new root"]);

        let commits = commit_log(repo.path(), Some(&old_head)).unwrap();
        assert_eq!(commits.len(), 1);
        assert_eq!(commits[0].message, "new root");
    }

    #[test]
    fn git_notes_write_show_and_list_round_trip() {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        run_git(repo.path(), &["config", "user.email", "test@example.test"]);
        std::fs::write(repo.path().join("README.md"), "one\n").unwrap();
        run_git(repo.path(), &["add", "README.md"]);
        run_git(repo.path(), &["commit", "-m", "note target"]);
        let head = current_head(repo.path()).unwrap();
        let notes_ref = "refs/notes/bbox-test/provenance";

        write_note(repo.path(), notes_ref, &head, "{\"ok\":true}\n").unwrap();
        write_note(repo.path(), notes_ref, &head, "{\"again\":true}\n").unwrap();

        let note = show_note(repo.path(), notes_ref, &head).unwrap().unwrap();
        assert!(note.contains("{\"ok\":true}"));
        assert!(note.contains(NOTE_DOCUMENT_SEPARATOR));
        assert!(note.contains("{\"again\":true}"));
        assert_eq!(list_notes(repo.path(), notes_ref).unwrap().len(), 1);
    }

    fn run_git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn init_repo(root: &Path) {
        run_git(root, &["init", "-q"]);
        run_git(root, &["config", "user.email", "t@example.com"]);
        run_git(root, &["config", "user.name", "Test"]);
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn object_oid(root: &Path, spec: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["rev-parse", spec])
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn write_raw_object(root: &Path, object_type: &str, bytes: &[u8]) -> String {
        let mut child = Command::new("git")
            .arg("-C")
            .arg(root)
            .args([
                "hash-object",
                "--literally",
                "-w",
                "-t",
                object_type,
                "--stdin",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "writing raw {object_type} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn raw_tree_entry(mode: &str, name: &str, object_id: &str) -> Vec<u8> {
        let mut entry = format!("{mode} {name}").into_bytes();
        entry.push(0);
        entry.extend(hex::decode(object_id).unwrap());
        entry
    }

    fn commit_root_tree(root: &Path, root_tree: &str) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["commit-tree", root_tree, "-m", "synthetic tree"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "committing raw tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }

    fn commit_scope_tree(root: &Path, scope_tree: &str) -> String {
        let root_tree =
            write_raw_object(root, "tree", &raw_tree_entry("40000", "scope", scope_tree));
        commit_root_tree(root, &root_tree)
    }

    #[test]
    fn read_committed_file_ignores_working_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/e1.json", "committed");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        // Dirty the working copy AFTER the commit.
        write(&root, ".bbox/knowledge/e1.json", "dirty-working-copy");

        assert_eq!(
            read_committed_file(&root, "HEAD", ".bbox/knowledge/e1.json").as_deref(),
            Some("committed"),
            "must read the committed blob, not the dirty working tree"
        );
        assert_eq!(
            read_committed_file(&root, "HEAD", ".bbox/knowledge/nope.json"),
            None,
            "absent path yields None, not empty string"
        );
    }

    #[test]
    fn bounded_committed_file_read_enforces_limit_before_retaining_full_blob() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/e1.json", "0123456789");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let commit = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        assert_eq!(
            read_verified_committed_file_bytes_bounded(&commit, ".bbox/knowledge/e1.json", 10,)
                .unwrap(),
            b"0123456789"
        );
        let error =
            read_verified_committed_file_bytes_bounded(&commit, ".bbox/knowledge/e1.json", 9)
                .unwrap_err();
        assert!(error.to_string().contains("exceeds its byte limit"));
        assert!(
            read_verified_committed_file_bytes_bounded(
                &commit,
                ".bbox/knowledge/missing.json",
                10,
            )
            .is_err()
        );
    }

    #[test]
    fn alternate_publisher_objects_support_independent_clone_overlay_reads() {
        let publisher_dir = tempfile::tempdir().unwrap();
        let publisher = publisher_dir.path().canonicalize().unwrap();
        init_repo(&publisher);
        write(&publisher, ".bbox/knowledge/first.json", "published-one");
        run_git(&publisher, &["add", "."]);
        run_git(&publisher, &["commit", "-q", "-m", "c1"]);

        let clone_parent = tempfile::tempdir().unwrap();
        let checkout = clone_parent.path().join("checkout");
        let output = Command::new("git")
            .args([
                "clone",
                "--no-local",
                "-q",
                publisher.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let checkout = checkout.canonicalize().unwrap();
        let checkout_head = resolve_commit(&checkout, "HEAD").unwrap();

        write(&publisher, ".bbox/knowledge/second.json", "published-two");
        run_git(&publisher, &["add", "."]);
        run_git(&publisher, &["commit", "-q", "-m", "c2"]);
        let publisher_head = resolve_commit(&publisher, "HEAD").unwrap();

        assert!(
            merge_base(&checkout, &checkout_head, &publisher_head).is_none(),
            "independent clone must not already contain the publisher's new object"
        );
        assert_eq!(
            merge_base_with_alternate(&checkout, &checkout_head, &publisher_head, Some(&publisher))
                .as_deref(),
            Some(checkout_head.as_str())
        );
        assert_eq!(
            read_committed_file_bytes_with_alternate(
                &checkout,
                &publisher_head,
                ".bbox/knowledge/second.json",
                Some(&publisher)
            )
            .as_deref(),
            Some(b"published-two".as_slice())
        );
        let verified =
            verify_commit_oid_with_alternate(&checkout, &publisher_head, Some(&publisher)).unwrap();
        let first_blob = object_oid(&publisher, "HEAD:.bbox/knowledge/first.json");
        let alternate_first_blob = publisher
            .join(".git/objects")
            .join(&first_blob[..2])
            .join(&first_blob[2..]);
        assert!(alternate_first_blob.is_file());
        fs::remove_file(alternate_first_blob).unwrap();
        assert_eq!(
            list_verified_committed_dir_bounded(&verified, ".bbox/knowledge", 10, 4096,).unwrap(),
            vec![
                ".bbox/knowledge/first.json".to_string(),
                ".bbox/knowledge/second.json".to_string(),
            ]
        );
        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/second.json",
                64,
            )
            .unwrap(),
            b"published-two"
        );
        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/first.json",
                64,
            )
            .unwrap(),
            b"published-one"
        );
    }

    #[test]
    fn list_committed_dir_lists_only_committed_blobs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/e1.json", "a");
        write(&root, ".bbox/knowledge/e2.json", "b");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        // An untracked working-tree file must NOT appear.
        write(&root, ".bbox/knowledge/e3.json", "c");

        let mut listed = list_committed_dir(&root, "HEAD", ".bbox/knowledge");
        listed.sort();
        assert_eq!(
            listed,
            vec![
                ".bbox/knowledge/e1.json".to_string(),
                ".bbox/knowledge/e2.json".to_string(),
            ]
        );
        assert!(
            list_committed_dir_result(&root, "refs/heads/missing", ".bbox/knowledge").is_err(),
            "strict callers must not mistake an invalid ref for an empty tree"
        );
    }

    #[test]
    fn bounded_committed_listing_enforces_bytes_count_and_ordering() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/z.json", "z");
        write(&root, ".bbox/knowledge/a.json", "a");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "c1"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let commit = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        let paths =
            list_verified_committed_dir_bounded(&commit, ".bbox/knowledge", 2, 4096).unwrap();
        assert_eq!(
            paths,
            vec![
                ".bbox/knowledge/a.json".to_string(),
                ".bbox/knowledge/z.json".to_string(),
            ]
        );
        assert!(
            list_verified_committed_dir_bounded(&commit, ".bbox/knowledge", 1, 4096)
                .unwrap_err()
                .to_string()
                .contains("entry limit")
        );
        assert!(
            list_verified_committed_dir_bounded(&commit, ".bbox/knowledge", 2, 1)
                .unwrap_err()
                .to_string()
                .contains("byte limit")
        );
    }

    #[test]
    fn bounded_committed_listing_rejects_regular_mode_pointing_to_tree() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let empty_tree = write_raw_object(&root, "tree", b"");
        let bad_scope = write_raw_object(
            &root,
            "tree",
            &raw_tree_entry("100644", "bad.json", &empty_tree),
        );
        let commit_oid = commit_scope_tree(&root, &bad_scope);
        let verified = verify_commit_oid_with_alternate(&root, &commit_oid, None).unwrap();

        let error = list_verified_committed_dir_bounded(&verified, "scope", 10, 4096).unwrap_err();
        assert!(error.to_string().contains("does not reference a blob"));
    }

    #[test]
    fn bounded_committed_listing_rejects_duplicate_raw_tree_names() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let blob = write_raw_object(&root, "blob", b"value");
        let mut duplicate_scope = raw_tree_entry("100644", "same.json", &blob);
        duplicate_scope.extend(raw_tree_entry("100644", "same.json", &blob));
        let duplicate_scope = write_raw_object(&root, "tree", &duplicate_scope);
        let commit_oid = commit_scope_tree(&root, &duplicate_scope);
        let verified = verify_commit_oid_with_alternate(&root, &commit_oid, None).unwrap();

        let error = list_verified_committed_dir_bounded(&verified, "scope", 10, 4096).unwrap_err();
        assert!(error.to_string().contains("duplicate entry name"));
    }

    #[test]
    fn bounded_committed_listing_limits_two_directory_deep_graphs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let mut child = write_raw_object(&root, "tree", b"");
        for _ in 0..16 {
            let mut tree = raw_tree_entry("40000", "a", &child);
            tree.extend(raw_tree_entry("40000", "b", &child));
            child = write_raw_object(&root, "tree", &tree);
        }
        let commit_oid = commit_scope_tree(&root, &child);
        let verified = verify_commit_oid_with_alternate(&root, &commit_oid, None).unwrap();

        let error =
            list_verified_committed_dir_bounded(&verified, "scope", 8, 1024 * 1024).unwrap_err();
        assert!(error.to_string().contains("tree count limit"));
    }

    #[test]
    fn raw_tree_prescan_rejects_single_tree_over_cap_before_next_insertion() {
        let object_id = "11".repeat(20);
        let mut tree = Vec::new();
        for index in 0..9 {
            tree.extend(raw_tree_entry(
                "100644",
                &format!("entry-{index}.json"),
                &object_id,
            ));
        }
        let mut raw_entry_count = 0_usize;
        let error = prescan_raw_tree_entries(&tree, 40, &mut raw_entry_count, 8).unwrap_err();
        assert!(error.to_string().contains("raw entry limit"));
        assert_eq!(
            raw_entry_count, 8,
            "the over-cap entry must be rejected before duplicate-set insertion"
        );
    }

    fn ls_tree_record(mode: &str, object_type: &str, path: &[u8]) -> Vec<u8> {
        let mut record = format!("{mode} {object_type} {}\t", "a".repeat(40)).into_bytes();
        record.extend_from_slice(path);
        record.push(0);
        record
    }

    #[test]
    fn bounded_committed_listing_rejects_malformed_paths() {
        let mut unterminated = ls_tree_record("100644", "blob", b".bbox/knowledge/good.json");
        unterminated.pop();
        let mut duplicate = ls_tree_record("100644", "blob", b".bbox/knowledge/a.json");
        duplicate.extend(ls_tree_record("100644", "blob", b".bbox/knowledge/a.json"));
        for malformed in [
            unterminated,
            ls_tree_record("100644", "blob", b".bbox/knowledge/../escape.json"),
            ls_tree_record("100644", "blob", b".bbox/knowledge/nested\\bad.json"),
            ls_tree_record("100644", "blob", b".bbox/knowledge/\xff.json"),
            ls_tree_record("100644", "blob", b"outside/file.json"),
            duplicate,
        ] {
            assert!(
                parse_bounded_committed_tree_paths(&malformed, ".bbox/knowledge", 10, 4096, 40,)
                    .is_err(),
                "malformed listing was accepted: {malformed:?}"
            );
        }
    }

    #[test]
    fn bounded_committed_listing_rejects_non_regular_entries() {
        for listing in [
            ls_tree_record("120000", "blob", b".bbox/knowledge/link.json"),
            ls_tree_record("160000", "commit", b".bbox/knowledge/module.json"),
            ls_tree_record("040000", "tree", b".bbox/knowledge/nested"),
            b"malformed metadata\t.bbox/knowledge/a.json\0".to_vec(),
        ] {
            assert!(
                parse_bounded_committed_tree_paths(&listing, ".bbox/knowledge", 10, 4096, 40,)
                    .is_err()
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn exact_listing_rejects_committed_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let knowledge = root.join(".bbox/knowledge");
        fs::create_dir_all(&knowledge).unwrap();
        symlink("target.json", knowledge.join("link.json")).unwrap();
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "symlink"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        assert!(
            list_verified_committed_dir_bounded(&verified, ".bbox/knowledge", 10, 4096)
                .unwrap_err()
                .to_string()
                .contains("non-regular-file")
        );
    }

    #[test]
    fn exact_commit_reads_ignore_ref_movement_and_replacement_objects() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "first");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "first"]);
        let first = resolve_commit(&root, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&root, &first, None).unwrap();

        write(&root, ".bbox/knowledge/entry.json", "second");
        run_git(&root, &["commit", "-q", "-am", "second"]);
        let second = resolve_commit(&root, "HEAD").unwrap();
        run_git(&root, &["replace", &first, &second]);

        assert_eq!(verified.oid(), first);
        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                16,
            )
            .unwrap(),
            b"first"
        );
        assert!(
            verify_commit_oid_with_alternate(&root, &first[..12], None).is_err(),
            "abbreviated object ids must not cross the exact-read boundary"
        );
    }

    #[test]
    fn exact_commit_verification_rejects_repository_configured_alternates() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, "entry", "one");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "one"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let info = root.join(".git/objects/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("alternates"), b"/untrusted/object/store\n").unwrap();

        assert!(verify_commit_oid_with_alternate(&root, &oid, None).is_err());
    }

    #[test]
    fn exact_commit_verification_rejects_nested_primary_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, "nested/entry", "one");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "one"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let nested = root.join("nested").canonicalize().unwrap();

        assert!(
            verify_commit_oid_with_alternate(&nested, &oid, None)
                .unwrap_err()
                .to_string()
                .contains("exact worktree root")
        );
    }

    #[test]
    fn exact_commit_verification_rejects_nested_explicit_alternate_root() {
        let primary_dir = tempfile::tempdir().unwrap();
        let primary = primary_dir.path().canonicalize().unwrap();
        init_repo(&primary);
        write(&primary, "entry", "primary");
        run_git(&primary, &["add", "."]);
        run_git(&primary, &["commit", "-q", "-m", "primary"]);
        let oid = resolve_commit(&primary, "HEAD").unwrap();

        let alternate_dir = tempfile::tempdir().unwrap();
        let alternate = alternate_dir.path().canonicalize().unwrap();
        init_repo(&alternate);
        write(&alternate, "nested/entry", "alternate");
        run_git(&alternate, &["add", "."]);
        run_git(&alternate, &["commit", "-q", "-m", "alternate"]);
        let nested_alternate = alternate.join("nested").canonicalize().unwrap();

        assert!(
            verify_commit_oid_with_alternate(&primary, &oid, Some(&nested_alternate))
                .unwrap_err()
                .to_string()
                .contains("exact worktree root")
        );
    }

    #[test]
    fn verified_commit_uses_captured_linked_worktree_gitdir() {
        let base_dir = tempfile::tempdir().unwrap();
        let base = base_dir.path().canonicalize().unwrap();
        init_repo(&base);
        write(&base, ".bbox/knowledge/entry.json", "worktree");
        run_git(&base, &["add", "."]);
        run_git(&base, &["commit", "-q", "-m", "base"]);

        let worktree_parent = tempfile::tempdir().unwrap();
        let worktree = worktree_parent.path().join("linked");
        run_git(
            &base,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "verified-worktree",
                worktree.to_str().unwrap(),
            ],
        );
        let worktree = worktree.canonicalize().unwrap();
        let oid = resolve_commit(&worktree, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&worktree, &oid, None).unwrap();
        assert!(
            verified
                .authority
                .primary
                .git_dir
                .file
                .metadata()
                .unwrap()
                .is_dir()
        );

        fs::rename(worktree.join(".git"), worktree.join(".git.detached")).unwrap();
        assert_eq!(
            list_verified_committed_dir_bounded(&verified, ".bbox/knowledge", 10, 4096).unwrap(),
            vec![".bbox/knowledge/entry.json".to_string()]
        );
        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                64,
            )
            .unwrap(),
            b"worktree"
        );
    }

    #[test]
    fn verified_commit_survives_checkout_root_replacement() {
        let container = tempfile::tempdir().unwrap();
        let root = container.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "authorized");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "authorized"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        let original = container.path().join("original-checkout");
        fs::rename(&root, &original).unwrap();
        fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "replacement");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "replacement"]);

        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                64,
            )
            .unwrap(),
            b"authorized"
        );
    }

    #[test]
    fn verified_commit_survives_gitdir_replacement() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "authorized");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "authorized"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        fs::rename(root.join(".git"), root.join(".git.authorized")).unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "replacement");
        run_git(&root, &["add", ".bbox/knowledge/entry.json"]);
        run_git(&root, &["commit", "-q", "-m", "replacement"]);

        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                64,
            )
            .unwrap(),
            b"authorized"
        );
    }

    #[test]
    fn verified_commit_ignores_alternates_added_after_verification() {
        let primary_temp = tempfile::tempdir().unwrap();
        let primary = primary_temp.path().canonicalize().unwrap();
        init_repo(&primary);
        write(&primary, ".bbox/knowledge/entry.json", "authorized");
        run_git(&primary, &["add", "."]);
        run_git(&primary, &["commit", "-q", "-m", "authorized"]);
        let oid = resolve_commit(&primary, "HEAD").unwrap();
        let blob = object_oid(&primary, "HEAD:.bbox/knowledge/entry.json");
        let verified = verify_commit_oid_with_alternate(&primary, &oid, None).unwrap();

        let untrusted_temp = tempfile::tempdir().unwrap();
        let untrusted = untrusted_temp.path().canonicalize().unwrap();
        init_repo(&untrusted);
        write(&untrusted, "entry.json", "authorized");
        run_git(&untrusted, &["add", "."]);
        run_git(&untrusted, &["commit", "-q", "-m", "untrusted"]);
        assert_eq!(object_oid(&untrusted, "HEAD:entry.json"), blob);
        let loose_blob = primary
            .join(".git/objects")
            .join(&blob[..2])
            .join(&blob[2..]);
        assert!(loose_blob.is_file());
        fs::remove_file(loose_blob).unwrap();
        fs::write(
            primary.join(".git/objects/info/alternates"),
            format!("{}\n", untrusted.join(".git/objects").display()),
        )
        .unwrap();

        assert!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                64,
            )
            .is_err(),
            "a verified read must fail closed instead of loading a late alternates file"
        );
    }

    #[test]
    fn verified_commit_survives_explicit_alternate_object_dir_replacement() {
        let publisher_temp = tempfile::tempdir().unwrap();
        let publisher = publisher_temp.path().canonicalize().unwrap();
        init_repo(&publisher);
        write(&publisher, ".bbox/knowledge/first.json", "first");
        run_git(&publisher, &["add", "."]);
        run_git(&publisher, &["commit", "-q", "-m", "first"]);

        let checkout_temp = tempfile::tempdir().unwrap();
        let checkout = checkout_temp.path().join("checkout");
        let clone = Command::new("git")
            .args([
                "clone",
                "--no-local",
                "-q",
                publisher.to_str().unwrap(),
                checkout.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone.status.success());
        let checkout = checkout.canonicalize().unwrap();

        write(&publisher, ".bbox/knowledge/second.json", "authorized");
        run_git(&publisher, &["add", "."]);
        run_git(&publisher, &["commit", "-q", "-m", "second"]);
        let oid = resolve_commit(&publisher, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&checkout, &oid, Some(&publisher)).unwrap();

        let replacement_temp = tempfile::tempdir().unwrap();
        let replacement = replacement_temp.path().canonicalize().unwrap();
        init_repo(&replacement);
        write(&replacement, "replacement", "objects");
        run_git(&replacement, &["add", "."]);
        run_git(&replacement, &["commit", "-q", "-m", "replacement"]);
        fs::rename(
            publisher.join(".git/objects"),
            publisher.join(".git/objects.authorized"),
        )
        .unwrap();
        fs::rename(
            replacement.join(".git/objects"),
            publisher.join(".git/objects"),
        )
        .unwrap();

        assert_eq!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/second.json",
                64,
            )
            .unwrap(),
            b"authorized"
        );
    }

    #[test]
    fn exact_listing_treats_special_pathspec_characters_literally() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        let tree_dir = ".bbox/knowledge/literal*?[x]!:scope";
        write(&root, &format!("{tree_dir}/entry.json"), "exact");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "special-path"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();
        let verified = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();

        assert_eq!(
            list_verified_committed_dir_bounded(&verified, tree_dir, 1, 4096).unwrap(),
            vec![format!("{tree_dir}/entry.json")]
        );
    }

    #[test]
    fn exact_blob_read_does_not_lazy_fetch_missing_promised_objects() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("checkout");
        fs::create_dir_all(&root).unwrap();
        init_repo(&root);
        write(&root, ".bbox/knowledge/entry.json", "promised");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "promised"]);
        let oid = resolve_commit(&root, "HEAD").unwrap();

        let remote = temp.path().join("remote.git");
        let clone = Command::new("git")
            .args([
                "clone",
                "--bare",
                "-q",
                root.to_str().unwrap(),
                remote.to_str().unwrap(),
            ])
            .output()
            .unwrap();
        assert!(clone.status.success());
        run_git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        run_git(&root, &["config", "core.repositoryformatversion", "1"]);
        run_git(&root, &["config", "remote.origin.promisor", "true"]);
        run_git(
            &root,
            &["config", "remote.origin.partialclonefilter", "blob:none"],
        );
        run_git(&root, &["config", "extensions.partialClone", "origin"]);

        let blob = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["rev-parse", "HEAD:.bbox/knowledge/entry.json"])
            .output()
            .unwrap();
        assert!(blob.status.success());
        let blob = String::from_utf8(blob.stdout).unwrap();
        let blob = blob.trim();
        let loose_blob = root.join(".git/objects").join(&blob[..2]).join(&blob[2..]);
        assert!(loose_blob.is_file());
        fs::remove_file(&loose_blob).unwrap();

        let verified = verify_commit_oid_with_alternate(&root, &oid, None).unwrap();
        assert!(
            read_verified_committed_file_bytes_bounded(
                &verified,
                ".bbox/knowledge/entry.json",
                64,
            )
            .is_err()
        );
        assert!(
            !loose_blob.exists(),
            "the hardened read must not fetch a missing promised blob"
        );
    }

    #[test]
    fn merge_base_finds_common_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        init_repo(&root);
        write(&root, "f.txt", "base");
        run_git(&root, &["add", "."]);
        run_git(&root, &["commit", "-q", "-m", "base"]);
        let base = current_head(&root).unwrap();

        // Diverge onto a branch and advance main.
        run_git(&root, &["checkout", "-q", "-b", "feature"]);
        write(&root, "f.txt", "feature");
        run_git(&root, &["commit", "-q", "-am", "feat"]);
        run_git(&root, &["checkout", "-q", "-"]);
        write(&root, "f.txt", "main2");
        run_git(&root, &["commit", "-q", "-am", "main2"]);

        assert_eq!(
            merge_base(&root, "HEAD", "feature").as_deref(),
            Some(base.as_str()),
            "merge-base of diverged branches is their common ancestor"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stable_repository_keeps_exact_authority_across_path_and_git_swaps() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let checkout = root.join("rehearsal/checkout");
        let outside = root.join("protected");
        fs::create_dir_all(&checkout).unwrap();
        fs::create_dir_all(&outside).unwrap();
        init_repo(&checkout);
        init_repo(&outside);
        write(&checkout, ".bbox/config.toml", "inside-authority\n");
        write(&checkout, "tracked.txt", "inside\n");
        run_git(&checkout, &["add", "."]);
        run_git(&checkout, &["commit", "-q", "-m", "inside"]);
        let inside_head = current_head(&checkout).unwrap();
        run_git(
            &checkout,
            &[
                "notes",
                "--ref",
                "refs/notes/stable-test",
                "add",
                "-m",
                "inside-note",
                &inside_head,
            ],
        );
        write(&outside, ".bbox/config.toml", "outside-sentinel\n");
        write(&outside, "tracked.txt", "outside\n");
        run_git(&outside, &["add", "."]);
        run_git(&outside, &["commit", "-q", "-m", "outside"]);
        let outside_head = current_head(&outside).unwrap();
        assert_ne!(inside_head, outside_head);

        let authority = NofollowDirectory::open_existing(&checkout)
            .unwrap()
            .unwrap();
        let repository = open_stable_git_repository(&authority).unwrap().unwrap();
        let first_generation = repository
            .snapshot_notes_generation_bounded("refs/notes/stable-test", 16, 64 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(first_generation.entries.len(), 1);
        assert_eq!(first_generation.entries[0].target_oid, inside_head);
        assert!(!first_generation.notes_tip.is_empty());
        let first_snapshot = repository
            .snapshot_notes_bounded("refs/notes/stable-test", 16, 64 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(first_snapshot.len(), 1);
        assert_eq!(first_snapshot[0].target_oid, inside_head);
        assert_eq!(first_snapshot[0].bytes, b"inside-note\n");

        run_git(
            &checkout,
            &[
                "notes",
                "--ref",
                "refs/notes/stable-test",
                "add",
                "-f",
                "-m",
                "moved-note",
                &inside_head,
            ],
        );
        assert_eq!(
            first_snapshot[0].bytes, b"inside-note\n",
            "an already captured immutable snapshot must not follow a moved notes ref"
        );
        let moved_snapshot = repository
            .snapshot_notes_bounded("refs/notes/stable-test", 16, 64 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(moved_snapshot[0].bytes, b"moved-note\n");

        let held_checkout = root.join("rehearsal/held-checkout");
        fs::rename(&checkout, &held_checkout).unwrap();
        symlink(&outside, &checkout).unwrap();
        fs::rename(held_checkout.join(".git"), held_checkout.join(".git-held")).unwrap();
        symlink(outside.join(".git"), held_checkout.join(".git")).unwrap();

        let head = repository.verified_head().unwrap().unwrap();
        assert_eq!(head.oid(), inside_head);
        assert_eq!(
            repository.resolve_commit_oid("HEAD").unwrap().as_deref(),
            Some(inside_head.as_str())
        );
        assert_eq!(
            repository.first_commit_oid(head.oid()).unwrap().as_deref(),
            Some(inside_head.as_str())
        );
        assert_eq!(
            read_verified_committed_file_bytes_bounded(&head, ".bbox/config.toml", 1024,).unwrap(),
            b"inside-authority\n"
        );
        let after_swap = repository
            .snapshot_notes_bounded("refs/notes/stable-test", 16, 64 * 1024)
            .unwrap()
            .unwrap();
        assert_eq!(after_swap[0].bytes, b"moved-note\n");
        assert!(after_swap.iter().all(|entry| {
            !entry
                .bytes
                .windows(16)
                .any(|window| window == b"outside-sentinel")
        }));
    }

    #[cfg(unix)]
    #[test]
    fn stable_repository_rejects_linked_worktree_git_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        fs::write(root.join(".git"), "gitdir: /outside/repository\n").unwrap();
        let authority = NofollowDirectory::open_existing(&root).unwrap().unwrap();
        assert!(open_stable_git_repository(&authority).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn stable_repository_rejects_configured_object_alternates() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        init_repo(&root);
        let info = root.join(".git/objects/info");
        fs::create_dir_all(&info).unwrap();
        fs::write(info.join("alternates"), "/outside/objects\n").unwrap();

        let authority = NofollowDirectory::open_existing(&root).unwrap().unwrap();
        let error = open_stable_git_repository(&authority)
            .expect_err("configured alternates must not enter stable authority");
        assert!(error.to_string().contains("object alternates"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn stable_directory_revalidation_rejects_a_rebound_path() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let authority_path = root.join("authority");
        let held_path = root.join("held-authority");
        fs::create_dir(&authority_path).unwrap();
        let authority = open_stable_directory(&authority_path, "test authority").unwrap();

        fs::rename(&authority_path, &held_path).unwrap();
        fs::create_dir(&authority_path).unwrap();

        assert!(authority.ensure_still_current().is_err());
    }

    #[test]
    fn git_notes_ref_components_are_structurally_confined() {
        for accepted in ["bbox", "team.notes", "team_notes", "team-notes"] {
            assert!(validate_notes_ref_component(accepted, "namespace").is_ok());
        }
        for rejected in [
            "",
            ".",
            "..",
            "-bbox",
            "bbox/",
            "bbox..notes",
            "bbox.",
            "bbox notes",
        ] {
            assert!(
                validate_notes_ref_component(rejected, "namespace").is_err(),
                "accepted {rejected:?}"
            );
        }
    }
}

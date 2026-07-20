use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

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
        return Ok(Vec::new());
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
pub fn set_notes_namespace(namespace: String) {
    let _ = NOTES_NAMESPACE_OVERRIDE.set(namespace);
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
pub fn notes_ref(kind: &str) -> String {
    format!("refs/notes/{}/{}", notes_namespace(), kind)
}

pub fn write_note(root: &Path, notes_ref: &str, commit: &str, body: &str) -> Result<()> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "notes",
            "--ref",
            notes_ref,
            "append",
            &format!("--separator={NOTE_DOCUMENT_SEPARATOR}"),
            "-F",
            "-",
            commit,
        ])
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning git notes append in {}", root.display()))?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(body.as_bytes())?;
    }
    let output = child.wait_with_output()?;
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
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["config", "notes.mergeStrategy", "union"])
        .output()
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

pub fn git_output(path: &Path, args: &[&str], action: &'static str) -> Option<Output> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(path).args(args);
    run_git_bounded(cmd, path, action)
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
    run_bounded_with_timeout(cmd, path, action, GIT_OUTPUT_TIMEOUT)
}

#[allow(clippy::disallowed_methods)]
fn run_bounded_with_timeout(
    mut cmd: Command,
    path: &Path,
    action: &'static str,
    timeout: Duration,
) -> Option<Output> {
    use std::io::Read;

    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
    let mut stdout_pipe = child.stdout.take()?;
    let mut stderr_pipe = child.stderr.take()?;
    let stdout_drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let stderr_drain = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
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
                let stdout = stdout_drain.join().unwrap_or_default();
                let stderr = stderr_drain.join().unwrap_or_default();
                return Some(Output {
                    status,
                    stdout,
                    stderr,
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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
}

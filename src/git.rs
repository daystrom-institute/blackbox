use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GitCommit {
    pub sha: String,
    pub parent_shas: Vec<String>,
    pub author_name: String,
    pub author_email: String,
    pub message: String,
}

pub(crate) fn git_root_for_path(path: &Path) -> Option<PathBuf> {
    let output = git_output(
        path,
        &["rev-parse", "--show-toplevel"],
        "deriving repository root",
    )?;
    if !output.status.success() {
        return None;
    }
    let root = String::from_utf8(output.stdout).ok()?;
    fs::canonicalize(root.trim()).ok()
}

pub(crate) fn git_first_commit_for_path(path: &Path) -> Option<String> {
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

pub(crate) fn git_first_commit_from_stdout(stdout: &[u8]) -> Option<String> {
    let raw = String::from_utf8(stdout.to_vec()).ok()?;
    let mut roots: Vec<&str> = raw
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    roots.sort_unstable();
    roots.first().map(|line| (*line).to_string())
}

pub(crate) fn git_remote_origin_for_path(path: &Path) -> Option<String> {
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

pub(crate) fn current_head(root: &Path) -> Option<String> {
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

pub(crate) fn commit_log(root: &Path, since_exclusive: Option<&str>) -> Result<Vec<GitCommit>> {
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
    let Some(merge_base) =
        git_output_strings(root, &merge_base_args, "checking git ancestry")
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

pub(crate) fn changed_files_for_commit(root: &Path, sha: &str) -> Result<Vec<String>> {
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

pub(crate) fn head_fingerprint(root: &Path) -> Option<u64> {
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

pub(crate) fn parse_commit_log(stdout: &[u8]) -> Result<Vec<GitCommit>> {
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

pub(crate) fn git_output(path: &Path, args: &[&str], action: &'static str) -> Option<Output> {
    match Command::new("git").arg("-C").arg(path).args(args).output() {
        Ok(output) => Some(output),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                action,
                "failed to execute git"
            );
            None
        }
    }
}

fn git_output_strings(path: &Path, args: &[String], action: &'static str) -> Option<Output> {
    match Command::new("git").arg("-C").arg(path).args(args).output() {
        Ok(output) => Some(output),
        Err(err) => {
            tracing::warn!(
                path = %path.display(),
                error = %err,
                action,
                "failed to execute git"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

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

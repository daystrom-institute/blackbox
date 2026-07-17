//! The `git` connector: mirrors a clone URL (plus optional `#<ref>`) into
//! the materialization root via the git CLI. Deliberate first connector
//! (operator-directed deviation from the design catalog's `local_mirror`-
//! first ordering) - the goal is agent-machine repos mirrored into the
//! cluster corpus host.
//!
//! ## Bulk materialization
//!
//! This connector reports `bulk_materializes() = true`: `materialize_bulk`
//! runs `git reset --hard <target_sha>` and lets git's own checkout
//! mechanics write the working tree, then reconciles the manifest against
//! the diff `list_changes` already computed. This means two known,
//! deliberate deviations from the generic per-entry driver path (both
//! exercised by the generic path's own tests in `manifest.rs`/`driver.rs`,
//! not claimed here):
//!
//! 1. **Path-safety encoding is not applied.** `physical_path` is set to
//!    the raw git-tracked path verbatim, matching exactly what `reset
//!    --hard` writes to disk. Git's own path model already forbids `..`
//!    and absolute components, so containment holds; the case-fold/NFC
//!    collision-disambiguation in `manifest::encode_physical_path` does
//!    NOT apply here, because a case-insensitive/normalizing local
//!    filesystem (APFS) cannot even hold two git-tracked paths that
//!    collide that way simultaneously - `git reset --hard` would already
//!    collapse them before this connector ever saw a diff.
//! 2. **Policy is enforced after checkout, not before fetch.** `git
//!    reset --hard` has already written bytes for every changed path by
//!    the time this connector reconciles the manifest; the driver still
//!    removes policy-excluded files and marks them skipped, so the
//!    materialization root and index never carry them, but the git-
//!    protocol transfer itself was not prevented (git does not support
//!    selective per-blob transfer without partial-clone filters, future
//!    work if this matters for a given mount).

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use async_trait::async_trait;

use crate::connector::{BulkMaterializeOutcome, RemoteSourceConnector};
use crate::manifest::{EntryState, Manifest, ManifestEntry};
use crate::materialize::{ensure_ownership_marker, sha256_hex};
use crate::types::{
    ChangeBatch, ChangeCursor, FetchedContent, MountConfig, RemoteChange, RemoteChangeKind,
    RemoteEntry, RemoteEntryKind, RemoteInfo,
};

#[derive(Debug, Default)]
pub struct GitConnector;

impl GitConnector {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl RemoteSourceConnector for GitConnector {
    fn kind(&self) -> &'static str {
        "git"
    }

    async fn validate(&self, mount: &MountConfig) -> Result<RemoteInfo> {
        let (url, want_ref) = split_ref(&mount.remote_scope);
        let target = want_ref.unwrap_or("HEAD");
        let output = run_git(&["ls-remote", "--symref", url, target], None)
            .await
            .with_context(|| {
                format!(
                    "probing `{}` via `git ls-remote`; verify the URL and that ambient git \
                     auth (ssh agent, credential helper, or a token already embedded in the \
                     URL) can reach it",
                    redact_scope(url)
                )
            })?;
        let (resolved_ref, sha) = parse_ls_remote(&output, want_ref).with_context(|| {
            format!(
                "resolving ref `{target}` on `{}` from `git ls-remote` output",
                redact_scope(url)
            )
        })?;
        let mut detail = BTreeMap::new();
        detail.insert("ref".to_string(), resolved_ref.clone());
        Ok(RemoteInfo {
            display_name: redact_scope(url),
            resolved_ref: Some(sha),
            detail,
        })
    }

    async fn list_changes(
        &self,
        mount: &MountConfig,
        cursor: Option<ChangeCursor>,
    ) -> Result<ChangeBatch> {
        let (url, want_ref) = split_ref(&mount.remote_scope);
        let root = &mount.materialization_root;
        ensure_clone_initialized(root, url).await?;

        let branch = match want_ref {
            Some(explicit) => explicit.to_string(),
            None => resolve_default_branch(root)
                .await
                .with_context(|| format!("resolving default branch on `{}`", redact_scope(url)))?,
        };
        run_git(&["fetch", "origin", &branch], Some(root))
            .await
            .with_context(|| format!("fetching `{branch}` from `{}`", redact_scope(url)))?;
        let target_sha = run_git(&["rev-parse", "FETCH_HEAD"], Some(root))
            .await?
            .trim()
            .to_string();

        let (changes, degraded, is_full_walk) = match &cursor {
            Some(ChangeCursor(sha)) if object_exists(root, sha).await => {
                let diff = diff_name_status(root, sha, &target_sha).await?;
                let blobs = blob_paths(root, &target_sha).await?;
                let changes = parse_name_status(&diff, &target_sha)?
                    .into_iter()
                    // Deletes stay: the path is absent from the target tree
                    // by definition, and a delete of a never-materialized
                    // gitlink no-ops at the manifest. Everything else must
                    // be a real blob (see blob_paths).
                    .filter(|change| {
                        matches!(change.kind, RemoteChangeKind::Deleted)
                            || blobs.contains(&change.entry.path)
                    })
                    .collect();
                (changes, false, false)
            }
            Some(_) => (full_walk_changes(root, &target_sha).await?, true, true),
            None => (full_walk_changes(root, &target_sha).await?, false, true),
        };

        Ok(ChangeBatch {
            changes,
            next_cursor: Some(ChangeCursor(target_sha)),
            degraded_to_full_walk: degraded,
            is_full_walk,
        })
    }

    async fn fetch(&self, mount: &MountConfig, entry: &RemoteEntry) -> Result<FetchedContent> {
        let root = &mount.materialization_root;
        let bytes = run_git_bytes(
            &["show", &format!("{}:{}", entry.remote_version, entry.path)],
            Some(root),
        )
        .await
        .with_context(|| format!("reading blob {}:{}", entry.remote_version, entry.path))?;
        Ok(FetchedContent {
            bytes,
            media_type: None,
            export_format: None,
        })
    }

    fn bulk_materializes(&self) -> bool {
        true
    }

    async fn materialize_bulk(
        &self,
        _mount: &MountConfig,
        root: &Path,
        manifest: &Manifest,
        batch: &ChangeBatch,
    ) -> Result<BulkMaterializeOutcome> {
        let Some(ChangeCursor(target_sha)) = &batch.next_cursor else {
            anyhow::bail!("git connector's change batch carried no next_cursor (target sha)");
        };
        run_git(&["reset", "--hard", target_sha], Some(root))
            .await
            .with_context(|| format!("resetting working tree to {target_sha}"))?;
        // `reset --hard` never touches untracked files, so our marker
        // ordinarily survives untouched; re-stamp defensively in case the
        // remote happens to track a file at the same path (a documented
        // edge case, not solved further here).
        ensure_ownership_marker(root)?;

        let mut outcome = BulkMaterializeOutcome::default();
        let mut live_paths: HashSet<&str> = HashSet::new();

        for change in &batch.changes {
            match &change.kind {
                RemoteChangeKind::Deleted => {
                    if let Some(existing) = manifest.find_by_logical_path(&change.entry.path) {
                        outcome.deleted_remote_ids.push(existing.remote_id.clone());
                    }
                }
                RemoteChangeKind::Renamed { from_path } => {
                    live_paths.insert(&change.entry.path);
                    let remote_id = manifest
                        .find_by_logical_path(from_path)
                        .map(|e| e.remote_id.clone())
                        .unwrap_or_else(|| path_remote_id(&change.entry.path));
                    outcome.upserts.push(
                        read_materialized_entry(root, &change.entry.path, remote_id, target_sha)
                            .await?,
                    );
                }
                RemoteChangeKind::Added | RemoteChangeKind::Modified => {
                    live_paths.insert(&change.entry.path);
                    outcome.upserts.push(
                        read_materialized_entry(
                            root,
                            &change.entry.path,
                            change.entry.remote_id.clone(),
                            target_sha,
                        )
                        .await?,
                    );
                }
            }
        }

        // A full walk's change list is exhaustive (every currently-live
        // path), so any manifest entry NOT in it is an orphan - most
        // visibly, files left over from a mount's target ref switching to
        // a branch that doesn't carry them. Ordinary incremental batches
        // are NOT exhaustive, so this only runs for full walks.
        if batch.is_full_walk {
            for existing in manifest.entries() {
                if !live_paths.contains(existing.logical_path.as_str()) {
                    outcome.deleted_remote_ids.push(existing.remote_id.clone());
                }
            }
        }

        Ok(outcome)
    }
}

async fn read_materialized_entry(
    root: &Path,
    path: &str,
    remote_id: String,
    target_sha: &str,
) -> Result<ManifestEntry> {
    let physical = root.join(path);
    let bytes = tokio::fs::read(&physical)
        .await
        .with_context(|| format!("reading checked-out file {}", physical.display()))?;
    Ok(ManifestEntry {
        remote_id,
        remote_version: target_sha.to_string(),
        logical_path: path.to_string(),
        // Verbatim, not encoded - see module docs §1.
        physical_path: path.to_string(),
        export_format: None,
        content_hash: Some(sha256_hex(&bytes)),
        state: EntryState::Materialized,
    })
}

/// Split a mount scope into `(url, ref)`. `ref` is whatever follows the
/// LAST `#` in the scope string; git URLs never legitimately contain `#`,
/// so this is unambiguous in practice.
fn split_ref(scope: &str) -> (&str, Option<&str>) {
    match scope.rsplit_once('#') {
        Some((url, r)) if !r.is_empty() => (url, Some(r)),
        _ => (scope, None),
    }
}

/// Redact userinfo (`user:pass@` / `token@`) from a scope's URL authority
/// before it can reach logs, `RemoteInfo`, or `SyncSummary`. Never persists
/// the credential-bearing form anywhere new - the operator's original URL
/// (with any embedded token) lives only in the caller-supplied
/// `MountConfig`, never in a store this crate writes.
pub fn redact_scope(scope: &str) -> String {
    let (url, r) = split_ref(scope);
    let redacted = redact_url(url);
    match r {
        Some(r) => format!("{redacted}#{r}"),
        None => redacted,
    }
}

fn redact_url(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let authority_and_rest = &url[scheme_end + 3..];
    let Some(at_pos) = authority_and_rest.find('@') else {
        return url.to_string();
    };
    let before_at = &authority_and_rest[..at_pos];
    // Only redact when '@' is part of the authority (before any '/'), not
    // an unrelated '@' further into the path.
    if before_at.contains('/') {
        return url.to_string();
    }
    format!(
        "{}://***@{}",
        &url[..scheme_end],
        &authority_and_rest[at_pos + 1..]
    )
}

async fn ensure_clone_initialized(root: &Path, url: &str) -> Result<()> {
    if !root.join(".git").exists() {
        run_git(&["init"], Some(root)).await.context("git init")?;
        run_git(&["remote", "add", "origin", url], Some(root))
            .await
            .context("git remote add origin")?;
        return Ok(());
    }
    if run_git(&["remote", "set-url", "origin", url], Some(root))
        .await
        .is_err()
    {
        run_git(&["remote", "add", "origin", url], Some(root))
            .await
            .context("git remote add origin")?;
    }
    Ok(())
}

async fn resolve_default_branch(root: &Path) -> Result<String> {
    let output = run_git(&["ls-remote", "--symref", "origin", "HEAD"], Some(root)).await?;
    let (branch, _sha) = parse_ls_remote(&output, None)?;
    Ok(branch)
}

async fn object_exists(root: &Path, sha: &str) -> bool {
    run_git(&["cat-file", "-e", sha], Some(root)).await.is_ok()
}

async fn diff_name_status(root: &Path, from: &str, to: &str) -> Result<String> {
    run_git(
        &[
            // Without quotepath=false, git C-quotes non-ASCII paths
            // (`"Ko\305\202..."` with literal quotes and octal escapes) and
            // the parsed path never matches any real file.
            "-c",
            "core.quotepath=false",
            "diff",
            "--name-status",
            "--find-renames",
            &format!("{from}..{to}"),
        ],
        Some(root),
    )
    .await
}

/// Paths that are real blobs at `sha`. Filters out gitlink (submodule)
/// entries - mode 160000, type `commit` - which `--name-only` and
/// name-status listings surface like files but which have no readable
/// content in the checkout (the working-tree path is a directory).
/// Submodules are deliberately not materialized: their content belongs to
/// a different repository, mountable as its own mount.
async fn blob_paths(root: &Path, sha: &str) -> Result<HashSet<String>> {
    let output = run_git(
        &["-c", "core.quotepath=false", "ls-tree", "-r", sha],
        Some(root),
    )
    .await?;
    let mut paths = HashSet::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let Some((meta, path)) = line.split_once('\t') else {
            continue;
        };
        let mut fields = meta.split_whitespace();
        let _mode = fields.next();
        if fields.next() == Some("blob") {
            paths.insert(path.to_string());
        }
    }
    Ok(paths)
}

async fn full_walk_changes(root: &Path, target_sha: &str) -> Result<Vec<RemoteChange>> {
    let mut paths: Vec<String> = blob_paths(root, target_sha).await?.into_iter().collect();
    paths.sort();
    Ok(paths
        .into_iter()
        .map(|path| RemoteChange {
            entry: make_entry(&path, target_sha),
            kind: RemoteChangeKind::Added,
        })
        .collect())
}

fn parse_name_status(output: &str, target_sha: &str) -> Result<Vec<RemoteChange>> {
    let mut changes = Vec::new();
    for line in output.lines() {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.split('\t');
        let status = fields
            .next()
            .ok_or_else(|| anyhow::anyhow!("malformed `git diff --name-status` line: {line:?}"))?;
        let code = status.chars().next().unwrap_or('?');
        match code {
            'A' | 'M' | 'T' => {
                let path = fields.next().ok_or_else(|| {
                    anyhow::anyhow!("missing path in `git diff --name-status` line: {line:?}")
                })?;
                let kind = if code == 'A' {
                    RemoteChangeKind::Added
                } else {
                    RemoteChangeKind::Modified
                };
                changes.push(RemoteChange {
                    entry: make_entry(path, target_sha),
                    kind,
                });
            }
            'D' => {
                let path = fields.next().ok_or_else(|| {
                    anyhow::anyhow!("missing path in `git diff --name-status` line: {line:?}")
                })?;
                changes.push(RemoteChange {
                    entry: make_entry(path, target_sha),
                    kind: RemoteChangeKind::Deleted,
                });
            }
            'R' | 'C' => {
                let from_path = fields.next().ok_or_else(|| {
                    anyhow::anyhow!("missing from-path in `git diff --name-status` line: {line:?}")
                })?;
                let to_path = fields.next().ok_or_else(|| {
                    anyhow::anyhow!("missing to-path in `git diff --name-status` line: {line:?}")
                })?;
                let kind = if code == 'R' {
                    RemoteChangeKind::Renamed {
                        from_path: from_path.to_string(),
                    }
                } else {
                    // A copy mints a new identity at the destination path;
                    // the source is untouched.
                    RemoteChangeKind::Added
                };
                changes.push(RemoteChange {
                    entry: make_entry(to_path, target_sha),
                    kind,
                });
            }
            other => {
                anyhow::bail!("unrecognized `git diff --name-status` code `{other}`: {line:?}")
            }
        }
    }
    Ok(changes)
}

fn make_entry(path: &str, target_sha: &str) -> RemoteEntry {
    RemoteEntry {
        remote_id: path_remote_id(path),
        path: path.to_string(),
        // Resolved from disk after `reset --hard` in `materialize_bulk`,
        // not known at enumeration time.
        size: None,
        remote_version: target_sha.to_string(),
        kind: RemoteEntryKind::File,
        media_type: None,
    }
}

/// Deterministic path-identity remote id. Git has no persistent per-file id
/// independent of path, so identity is path-derived; the driver/connector
/// rename reconciliation carries the OLD id forward across a detected
/// rename rather than trusting this freshly minted value for the new path.
fn path_remote_id(path: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(b"git-path:");
    hasher.update(path.as_bytes());
    hex::encode(&hasher.finalize()[..8])
}

/// Parse `git ls-remote --symref <target> <ref-or-HEAD>` output. When
/// `want_ref` is `None`, resolves the symbolic default branch name (from
/// the `ref: refs/heads/<name>\tHEAD` line) and its sha. When `Some`, takes
/// the first matching line's sha and echoes the requested name back as the
/// resolved ref.
fn parse_ls_remote(output: &str, want_ref: Option<&str>) -> Result<(String, String)> {
    let mut symref_target: Option<String> = None;
    let mut sha_for_symref: Option<String> = None;
    let mut first: Option<(String, String)> = None;

    for line in output.lines() {
        if let Some(rest) = line.strip_prefix("ref: ") {
            if let Some((target, _)) = rest.split_once('\t') {
                symref_target = Some(target.to_string());
            }
            continue;
        }
        let Some((sha, refname)) = line.split_once('\t') else {
            continue;
        };
        if first.is_none() {
            first = Some((sha.to_string(), refname.to_string()));
        }
        if symref_target.as_deref() == Some(refname) {
            sha_for_symref = Some(sha.to_string());
        }
    }

    match want_ref {
        None => {
            let branch = symref_target
                .as_deref()
                .and_then(|r| r.strip_prefix("refs/heads/"))
                .map(str::to_string)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "could not resolve the remote default branch from `git ls-remote \
                         --symref` output (no HEAD symref line)"
                    )
                })?;
            let sha = sha_for_symref
                .or_else(|| first.map(|(sha, _)| sha))
                .ok_or_else(|| anyhow::anyhow!("remote reported no sha for HEAD"))?;
            Ok((branch, sha))
        }
        Some(explicit) => {
            let (sha, _refname) = first
                .ok_or_else(|| anyhow::anyhow!("ref `{explicit}` was not found on the remote"))?;
            Ok((explicit.to_string(), sha))
        }
    }
}

async fn run_git(args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let bytes = run_git_bytes(args, cwd).await?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

async fn run_git_bytes(args: &[&str], cwd: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdin(std::process::Stdio::null());
    // Error text redacts credential-bearing args AND their echoes in git's
    // stderr: fetch/ls-remote/set-url receive the URL as an argument, git
    // repeats it verbatim in messages like "unable to access", and the
    // inner error survives in the anyhow chain beneath any redacted outer
    // context.
    let shown = || {
        args.iter()
            .map(|a| redact_url(a))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let output = cmd
        .output()
        .await
        .with_context(|| format!("running `git {}`", shown()))?;
    if !output.status.success() {
        let mut stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        for arg in args {
            let redacted = redact_url(arg);
            if redacted != **arg {
                stderr = stderr.replace(*arg, &redacted);
            }
        }
        anyhow::bail!("git {} failed: {}", shown(), stderr);
    }
    Ok(output.stdout)
}

#[cfg(test)]
// Fixture repo setup and content assertions use plain std::fs/git-CLI
// synchronously - test-only harness, unrelated to the connector's own
// tokio::process-based async discipline.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use crate::driver::sync_mount;
    use crate::policy::MountPolicy;
    use std::collections::BTreeMap;

    // Fixture repo setup is a synchronous, test-only git harness -
    // unrelated to the async library code under test.
    #[allow(clippy::disallowed_methods)]
    fn run_fixture(path: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[allow(clippy::disallowed_methods)]
    fn init_fixture_repo(path: &Path, branch: &str) {
        std::fs::create_dir_all(path).unwrap();
        run_fixture(path, &["init", "-b", branch]);
    }

    #[allow(clippy::disallowed_methods)]
    fn write_and_commit(path: &Path, rel: &str, content: &str, msg: &str) {
        let file = path.join(rel);
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&file, content).unwrap();
        run_fixture(path, &["add", rel]);
        run_fixture(path, &["commit", "-m", msg]);
    }

    #[allow(clippy::disallowed_methods)]
    fn delete_and_commit(path: &Path, rel: &str, msg: &str) {
        run_fixture(path, &["rm", rel]);
        run_fixture(path, &["commit", "-m", msg]);
    }

    #[allow(clippy::disallowed_methods)]
    fn rename_and_commit(path: &Path, from: &str, to: &str, msg: &str) {
        if let Some(parent) = path.join(to).parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        run_fixture(path, &["mv", from, to]);
        run_fixture(path, &["commit", "-m", msg]);
    }

    fn file_url(path: &Path) -> String {
        format!("file://{}", path.display())
    }

    fn mount(scope: String, root: &Path) -> MountConfig {
        MountConfig {
            connector_kind: "git".to_string(),
            remote_scope: scope,
            materialization_root: root.to_path_buf(),
            policy: MountPolicy::default(),
            options: BTreeMap::new(),
        }
    }

    #[test]
    fn scope_splits_url_and_ref() {
        assert_eq!(split_ref("https://x/y.git"), ("https://x/y.git", None));
        assert_eq!(
            split_ref("https://x/y.git#main"),
            ("https://x/y.git", Some("main"))
        );
    }

    #[test]
    fn redact_scope_hides_userinfo_but_keeps_shape() {
        let scope = "https://token123:x-oauth-basic@example.invalid/repo.git#main";
        let redacted = redact_scope(scope);
        assert!(!redacted.contains("token123"));
        assert_eq!(redacted, "https://***@example.invalid/repo.git#main");
    }

    #[test]
    fn redact_scope_is_a_no_op_without_userinfo() {
        let scope = "https://example.invalid/repo.git#main";
        assert_eq!(redact_scope(scope), scope);
    }

    #[test]
    fn parse_ls_remote_resolves_default_branch() {
        let output = "ref: refs/heads/main\tHEAD\nabc123\tHEAD\nabc123\trefs/heads/main\n";
        let (branch, sha) = parse_ls_remote(output, None).unwrap();
        assert_eq!(branch, "main");
        assert_eq!(sha, "abc123");
    }

    #[test]
    fn parse_ls_remote_resolves_explicit_ref() {
        let output = "def456\trefs/heads/feature\n";
        let (branch, sha) = parse_ls_remote(output, Some("feature")).unwrap();
        assert_eq!(branch, "feature");
        assert_eq!(sha, "def456");
    }

    #[test]
    fn parse_name_status_handles_all_status_codes() {
        let output = "A\tadded.txt\nM\tmodified.txt\nD\tdeleted.txt\nR100\told.txt\tnew.txt\n";
        let changes = parse_name_status(output, "sha").unwrap();
        assert_eq!(changes.len(), 4);
        assert_eq!(changes[0].kind, RemoteChangeKind::Added);
        assert_eq!(changes[1].kind, RemoteChangeKind::Modified);
        assert_eq!(changes[2].kind, RemoteChangeKind::Deleted);
        assert_eq!(
            changes[3].kind,
            RemoteChangeKind::Renamed {
                from_path: "old.txt".to_string()
            }
        );
        assert_eq!(changes[3].entry.path, "new.txt");
    }

    #[tokio::test]
    async fn non_ascii_paths_materialize_unquoted() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        // Non-ASCII filename: git C-quotes this in ls-tree/diff output
        // unless core.quotepath=false, and the quoted form matches no file.
        write_and_commit(
            &remote,
            "attachments/faktura_Kołodziejczyk.pdf",
            "pdfish\n",
            "add non-ascii path",
        );

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);
        let connector = GitConnector::new();

        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        assert_eq!(summary.fetched, 1);
        assert!(
            root.join("attachments/faktura_Kołodziejczyk.pdf").is_file(),
            "non-ascii path must materialize under its real name"
        );

        // Incremental leg: modify the file and re-sync from the cursor.
        write_and_commit(
            &remote,
            "attachments/faktura_Kołodziejczyk.pdf",
            "pdfish v2\n",
            "modify non-ascii path",
        );
        let summary2 = sync_mount(&connector, &mount, &manifest_path, summary.next_cursor)
            .await
            .unwrap();
        assert!(summary2.errors.is_empty(), "{:?}", summary2.errors);
        assert_eq!(summary2.fetched, 1);
    }

    #[tokio::test]
    async fn submodule_gitlinks_are_skipped_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let sub = base.join("sub");
        init_fixture_repo(&sub, "main");
        write_and_commit(&sub, "inner.txt", "inner\n", "add inner");

        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");
        // Gitlink entry: `ls-tree` reports it mode 160000 / type commit, and
        // the checked-out path is a directory - the exact shape that made
        // bulk materialize fail before the blob filter.
        run_fixture(
            &remote,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                sub.to_str().unwrap(),
                "vendored",
            ],
        );
        run_fixture(&remote, &["commit", "-m", "add submodule"]);

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);
        let connector = GitConnector::new();

        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert!(summary.errors.is_empty(), "{:?}", summary.errors);
        // a.txt + .gitmodules are blobs; the `vendored` gitlink is not.
        assert_eq!(summary.fetched, 2);
        let manifest = crate::manifest::Manifest::open(&manifest_path).unwrap();
        assert!(
            manifest
                .entries()
                .all(|e| !e.logical_path.starts_with("vendored")),
            "gitlink path must not enter the manifest"
        );
    }

    #[tokio::test]
    async fn initial_sync_materializes_default_branch_and_excludes_git_dir() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);
        let connector = GitConnector::new();

        let summary = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(summary.fetched, 1);
        assert!(summary.errors.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "hello\n"
        );

        let manifest = crate::manifest::Manifest::open(&manifest_path).unwrap();
        assert_eq!(manifest.entries().count(), 1);
        assert!(
            manifest
                .entries()
                .all(|e| !e.logical_path.starts_with(".git"))
        );
    }

    #[tokio::test]
    async fn incremental_sync_handles_add_modify_delete() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");
        write_and_commit(&remote, "b.txt", "bye\n", "add b");

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);
        let connector = GitConnector::new();

        let first = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(first.fetched, 2);

        write_and_commit(&remote, "a.txt", "hello again\n", "modify a");
        delete_and_commit(&remote, "b.txt", "delete b");
        write_and_commit(&remote, "c.txt", "new file\n", "add c");

        let second = sync_mount(
            &connector,
            &mount,
            &manifest_path,
            first.next_cursor.clone(),
        )
        .await
        .unwrap();
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        assert_eq!(second.deleted, 1);
        assert_eq!(second.fetched, 2); // modified a.txt + added c.txt

        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "hello again\n"
        );
        assert!(!root.join("b.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("c.txt")).unwrap(),
            "new file\n"
        );
    }

    #[tokio::test]
    async fn incremental_sync_pure_rename_reconciles_same_remote_id() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);
        let connector = GitConnector::new();

        let first = sync_mount(&connector, &mount, &manifest_path, None)
            .await
            .unwrap();
        let remote_id_before = crate::manifest::Manifest::open(&manifest_path)
            .unwrap()
            .find_by_logical_path("a.txt")
            .unwrap()
            .remote_id
            .clone();

        rename_and_commit(&remote, "a.txt", "renamed/a.txt", "rename a");

        let second = sync_mount(
            &connector,
            &mount,
            &manifest_path,
            first.next_cursor.clone(),
        )
        .await
        .unwrap();
        assert_eq!(second.renamed, 1);
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("renamed/a.txt")).unwrap(),
            "hello\n"
        );

        let manifest_after = crate::manifest::Manifest::open(&manifest_path).unwrap();
        assert!(manifest_after.find_by_logical_path("a.txt").is_none());
        let after = manifest_after
            .find_by_logical_path("renamed/a.txt")
            .unwrap();
        assert_eq!(after.remote_id, remote_id_before);
    }

    #[tokio::test]
    async fn cursor_resume_across_fresh_driver_instance_skips_refetch() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");
        let mount = mount(file_url(&remote), &root);

        let first = sync_mount(&GitConnector::new(), &mount, &manifest_path, None)
            .await
            .unwrap();
        assert_eq!(first.fetched, 1);

        // A brand new connector instance and a manifest reloaded fresh
        // from disk - no in-process state carries over.
        let second = sync_mount(
            &GitConnector::new(),
            &mount,
            &manifest_path,
            first.next_cursor.clone(),
        )
        .await
        .unwrap();
        assert_eq!(second.fetched, 0);
        assert_eq!(second.deleted, 0);
        assert_eq!(second.renamed, 0);
        assert_eq!(second.next_cursor, first.next_cursor);
    }

    #[tokio::test]
    async fn ref_switch_materializes_new_branch_and_removes_orphans() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "on main\n", "add a on main");
        run_fixture(&remote, &["checkout", "-b", "other"]);
        run_fixture(&remote, &["rm", "a.txt"]);
        write_and_commit(&remote, "b.txt", "on other\n", "add b on other");
        run_fixture(&remote, &["checkout", "main"]);

        let root = base.join("mount-root");
        let manifest_path = base.join("manifest.json");

        let mount_main = mount(file_url(&remote), &root);
        sync_mount(&GitConnector::new(), &mount_main, &manifest_path, None)
            .await
            .unwrap();
        assert!(root.join("a.txt").exists());

        // Ref switch: a full resync (cursor=None) against the new ref.
        let mount_other = mount(format!("{}#other", file_url(&remote)), &root);
        let summary = sync_mount(&GitConnector::new(), &mount_other, &manifest_path, None)
            .await
            .unwrap();

        assert!(root.join("b.txt").exists());
        assert!(
            !root.join("a.txt").exists(),
            "orphaned file from the old ref must be removed"
        );
        assert_eq!(summary.deleted, 1);

        let manifest = crate::manifest::Manifest::open(&manifest_path).unwrap();
        assert!(manifest.find_by_logical_path("a.txt").is_none());
        assert!(manifest.find_by_logical_path("b.txt").is_some());
    }

    #[tokio::test]
    async fn validate_resolves_default_branch_and_sha() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let remote = base.join("remote");
        init_fixture_repo(&remote, "main");
        write_and_commit(&remote, "a.txt", "hello\n", "add a");

        let mount = mount(file_url(&remote), &base.join("unused-root"));
        let info = GitConnector::new().validate(&mount).await.unwrap();
        assert_eq!(info.detail.get("ref").map(String::as_str), Some("main"));
        assert!(info.resolved_ref.is_some());
    }

    #[tokio::test]
    async fn validate_fails_closed_with_remediation_text_for_bad_url() {
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        let mount = mount(
            file_url(&base.join("does-not-exist")),
            &base.join("unused-root"),
        );
        let err = GitConnector::new().validate(&mount).await.unwrap_err();
        let message = format!("{err:#}");
        assert!(message.contains("probing"), "{message}");
    }
}

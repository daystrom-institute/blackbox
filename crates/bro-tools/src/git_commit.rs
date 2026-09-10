//! Commit explicit files without consuming another agent's staged work.

use crate::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use anyhow::{Context, bail};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

#[derive(Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GitCommitInput {
    /// Commit message.
    message: String,
    /// Literal files relative to the Git repository root, including deletions.
    /// Directories, globs, pathspec magic, symlinks and traversal are refused.
    paths: Vec<String>,
}

pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &str {
        "git_commit"
    }

    fn description(&self) -> &str {
        "Commit the current worktree contents of explicit, literal repository-relative files (including new files and deletions). Refuses directories, globs, pathspec magic, symlinks and sensitive paths. Refuses if other files are already staged; leave other agents' staging alone and use an isolated worktree. Commits only the named files, including all their unstaged changes. At most 256 files and 120 seconds total; interrupted or incomplete receipts disclose potentially changed index/commit state."
    }

    fn input_schema(&self) -> Value {
        schema_for::<GitCommitInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(error) => return ToolResult::Error(format!("bad input: {error}")),
        };
        match commit(cx, args).await {
            Ok(output) => ToolResult::Text(output),
            Err(error) => ToolResult::Error(format!("{error:#}")),
        }
    }
}

const COMMIT_TIME: std::time::Duration = std::time::Duration::from_secs(120);
const GIT_CAPTURE_BYTES: usize = 1024 * 1024;

async fn git_at(
    cx: &ToolCx,
    root: &Path,
    args: &[std::ffi::OsString],
    until: tokio::time::Instant,
) -> anyhow::Result<Vec<u8>> {
    let output =
        crate::workspace::workspace_git::capture_git(cx, root, args, until, GIT_CAPTURE_BYTES)
            .await
            .map_err(anyhow::Error::msg)?;
    if output.exit_code != Some(0) || !output.complete() {
        let cleanup_note = if output.cancelled || output.timed_out {
            " Forced termination may leave Git lock files; inspect the repository before retrying."
        } else {
            ""
        };
        bail!(
            "Git command did not return a complete success receipt; any attempted index or commit mutation may have completed or partially changed state. No rollback was attempted.{cleanup_note} {}\n{}{}",
            output.facts(),
            crate::output::truncate_text(&String::from_utf8_lossy(&output.stdout), 2000),
            crate::output::truncate_text(&String::from_utf8_lossy(&output.stderr), 2000)
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
async fn git(cx: &ToolCx, root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    git_at(
        cx,
        root,
        &args
            .iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
        tokio::time::Instant::now() + COMMIT_TIME,
    )
    .await
}

fn validate_literal(path: &str, safety: &crate::SafetyPolicy) -> anyhow::Result<()> {
    if path.is_empty()
        || Path::new(path).is_absolute()
        || path.starts_with(':')
        || path.contains(['*', '?', '[', ']', '\\', '\0'])
        || path.split('/').any(|part| {
            part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git")
        })
    {
        bail!("refused {path:?}: supply explicit literal repository-relative files");
    }
    let mut prefix = PathBuf::new();
    for part in path.split('/') {
        prefix.push(part);
        if safety.is_sensitive_path(&prefix)
            || safety.is_sensitive_path(Path::new(&prefix.to_string_lossy().to_lowercase()))
        {
            bail!("refused {path:?}: looks like a secret/credential");
        }
    }
    Ok(())
}

// Both ls-files --stage and ls-tree separate metadata from a literal path
// with a tab, and terminate records with NUL. Never parse filenames by lines.
fn path_modes(output: &[u8]) -> anyhow::Result<BTreeMap<&[u8], &[u8]>> {
    output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
        .map(|record| {
            let tab = record
                .iter()
                .position(|byte| *byte == b'\t')
                .context("git path record missing tab")?;
            let space = record
                .iter()
                .position(|byte| *byte == b' ')
                .context("git path record missing mode")?;
            Ok((&record[tab + 1..], &record[..space]))
        })
        .collect()
}

async fn commit(cx: &ToolCx, args: GitCommitInput) -> anyhow::Result<String> {
    let until = tokio::time::Instant::now() + COMMIT_TIME;
    let git = |root: &Path, args: &[&str]| {
        let root = root.to_path_buf();
        let args: Vec<std::ffi::OsString> = args.iter().map(std::ffi::OsString::from).collect();
        async move { git_at(cx, &root, &args, until).await }
    };
    if args.paths.len() > 256 {
        bail!("paths is limited to 256 explicit files per commit");
    }
    if args.paths.is_empty() {
        bail!("paths must name explicit files; refusing an empty selection");
    }
    if args.message.trim().is_empty() {
        bail!("message must not be empty");
    }
    for path in &args.paths {
        validate_literal(path, &cx.safety)?;
    }
    let effective_root = crate::workspace::effective_root(&cx.root);
    let root_output = git(&effective_root, &["rev-parse", "--show-toplevel"]).await?;
    let root_text = std::str::from_utf8(&root_output).context("repository path is not UTF-8")?;
    let root = PathBuf::from(root_text.strip_suffix('\n').unwrap_or(root_text));
    let paths: BTreeSet<&str> = args.paths.iter().map(String::as_str).collect();

    if !git(&root, &["ls-files", "--unmerged", "-z"])
        .await?
        .is_empty()
    {
        bail!("refused: resolve the repository's unmerged index entries before committing");
    }
    let staged = git(
        &root,
        &["diff", "--cached", "--name-only", "--no-renames", "-z"],
    )
    .await?;
    let foreign: Vec<_> = staged
        .split(|byte| *byte == 0)
        .filter(|path| {
            !path.is_empty() && !paths.iter().any(|selected| selected.as_bytes() == *path)
        })
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect();
    if !foreign.is_empty() {
        bail!(
            "refused: files outside paths are already staged: {foreign:?}. Leave their staging intact; use an isolated worktree."
        );
    }

    let mut index_args = vec!["ls-files", "--stage", "-z", "--"];
    index_args.extend(paths.iter().copied());
    let index_output = git(&root, &index_args).await?;
    let index = path_modes(&index_output)?;
    // HEAD is absent in a new repository. Only skip the tree read in that case.
    let head_probe = crate::workspace::workspace_git::capture_git(
        cx,
        &root,
        &["rev-parse", "--verify", "--quiet", "HEAD"]
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
        until,
        GIT_CAPTURE_BYTES,
    )
    .await
    .map_err(anyhow::Error::msg)?;
    if !head_probe.complete() || !matches!(head_probe.exit_code, Some(0 | 1)) {
        bail!("could not determine HEAD safely: {}", head_probe.facts());
    }
    let head_output = if head_probe.exit_code == Some(0) {
        let mut tree_args = vec!["ls-tree", "-r", "-z", "HEAD", "--"];
        tree_args.extend(paths.iter().copied());
        git(&root, &tree_args).await?
    } else {
        Vec::new()
    };
    let head = path_modes(&head_output)?;
    let mut stage = Vec::new();
    for path in &paths {
        let known_mode = index
            .get(path.as_bytes())
            .or_else(|| head.get(path.as_bytes()));
        if known_mode.is_some_and(|mode| *mode == b"160000") {
            bail!("refused {path:?}: submodules are not individual files");
        }
        let mut absolute = root.clone();
        let parts: Vec<_> = path.split('/').collect();
        let mut present = false;
        for (position, part) in parts.iter().enumerate() {
            absolute.push(part);
            let last = position + 1 == parts.len();
            match tokio::fs::symlink_metadata(&absolute).await {
                Ok(metadata) => {
                    if metadata.file_type().is_symlink() {
                        bail!("refused {path:?}: symlinks and symlink traversal are unsupported");
                    }
                    if (last && !metadata.is_file()) || (!last && !metadata.is_dir()) {
                        bail!("refused {path:?}: select individual regular files, not directories");
                    }
                    present = last;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
                Err(error) => return Err(error).with_context(|| format!("inspect {path:?}")),
            }
        }
        if !present && known_mode.is_none() {
            bail!("refused {path:?}: file does not exist and is not a tracked deletion");
        }
        // An already-staged deletion no longer exists in the index. Re-adding
        // it fails, but commit --only can select it from HEAD directly.
        if present || index.contains_key(path.as_bytes()) {
            stage.push(*path);
        }
    }

    // Validate the whole selection before changing the index. Git takes its
    // own index lock. --only is also essential: another process can stage
    // unrelated files after our preflight, and a plain commit would absorb them.
    if !stage.is_empty() {
        let mut add = vec!["add", "--"];
        add.extend(stage);
        git(&root, &add).await?;
    }
    commit_only(cx, &root, &args.message, paths, until).await
}

async fn commit_only(
    cx: &ToolCx,
    root: &Path,
    message: &str,
    paths: BTreeSet<&str>,
    until: tokio::time::Instant,
) -> anyhow::Result<String> {
    let mut commit_args = vec!["commit", "--only", "-m", message, "--"];
    commit_args.extend(paths);
    let output = git_at(
        cx,
        root,
        &commit_args
            .into_iter()
            .map(std::ffi::OsString::from)
            .collect::<Vec<_>>(),
        until,
    )
    .await
    .context(
        "commit did not return a complete success receipt; earlier staging or a commit may have changed repository state. No rollback was attempted",
    )?;
    let invalid = std::str::from_utf8(&output).is_err();
    let mut text = String::from_utf8_lossy(&output).into_owned();
    if invalid {
        text.push_str(
            "\n[git commit output contained invalid UTF-8; replacement characters were used]\n",
        );
    }
    let cap = if cx.output_budget == 0 {
        8000
    } else {
        cx.output_budget.min(8000)
    };
    Ok(crate::output::truncate_text(&text, cap))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::{Arc, Mutex};

    struct Fixture {
        _dir: tempfile::TempDir,
        cx: ToolCx,
    }

    impl Fixture {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let cx = ToolCx {
                tool_observations: Default::default(),
                instruction_generation: 0,
                instruction_policy: None,
                root,
                safety: Arc::new(crate::SafetyPolicy::new()),
                http: reqwest::Client::new(),
                todos: Arc::new(Mutex::new(Default::default())),
                shell_sessions: Arc::new(Mutex::new(Default::default())),
                edits: Arc::new(Mutex::new(Default::default())),
                session_env: Arc::new(Default::default()),
                shell_env: Arc::new(Default::default()),
                cancellation: Default::default(),
                output_budget: 16 * 1024,
                child_env: Arc::new(Default::default()),
                tool_arg_defaults: Arc::new(Default::default()),
            };
            git(&cx, &cx.root, &["init", "-q"]).await.unwrap();
            for (key, value) in [
                ("user.name", "Test"),
                ("user.email", "test@example.invalid"),
                ("commit.gpgsign", "false"),
            ] {
                git(&cx, &cx.root, &["config", "--local", key, value])
                    .await
                    .unwrap();
            }
            Self { _dir: dir, cx }
        }

        async fn write(&self, path: &str, body: &str) {
            let full = self.cx.root.join(path);
            tokio::fs::create_dir_all(full.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(full, body).await.unwrap();
        }

        async fn commit(&self, paths: &[&str]) -> ToolResult {
            GitCommit
                .call(json!({"message": "test change", "paths": paths}), &self.cx)
                .await
        }

        async fn git(&self, args: &[&str]) -> Vec<u8> {
            git(&self.cx, &self.cx.root, args).await.unwrap()
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancelled_commit_hook_is_reaped_and_reports_potential_index_change() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new().await;
        fixture.write("mine.txt", "selected").await;
        fixture
            .write(
                ".git/hooks/pre-commit",
                "#!/bin/sh\nprintf started > hook-started\nsleep 30\nprintf leaked > hook-leaked\n",
            )
            .await;
        tokio::fs::set_permissions(
            fixture.cx.root.join(".git/hooks/pre-commit"),
            std::fs::Permissions::from_mode(0o755),
        )
        .await
        .unwrap();
        let cx = fixture.cx.clone();
        let root = cx.root.clone();
        let cancel = cx.cancellation.clone();
        let invocation = tokio::spawn(async move {
            GitCommit
                .call(json!({"message":"bounded hook", "paths":["mine.txt"]}), &cx)
                .await
        });
        let until = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while !root.join("hook-started").exists() && tokio::time::Instant::now() < until {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(root.join("hook-started").exists());
        cancel.cancel();
        let result = tokio::time::timeout(std::time::Duration::from_secs(3), invocation)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_error());
        let text = result.into_content().0;
        assert!(
            text.contains("cancelled") && text.contains("No rollback"),
            "{text}"
        );
        assert!(!root.join("hook-leaked").exists());
        // Forced termination may leave Git lock files; the receipt does not claim rollback.
        assert!(text.contains("lock files"));
        assert!(text.contains("a commit may have changed repository state"));
        assert!(!text.contains("staging remains"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commit_hooks_receive_explicit_project_environment_after_spawn() {
        use std::os::unix::fs::PermissionsExt;
        let mut fixture = Fixture::new().await;
        fixture.cx.shell_env = Arc::new(BTreeMap::from([(
            "AUDIT_PROJECT_FLAG".into(),
            "expected".into(),
        )]));
        fixture.cx.child_env =
            Arc::new(crate::ChildEnvironment::new(["AUDIT_PROJECT_FLAG".into()]));
        fixture.write("mine.txt", "selected").await;
        fixture
            .write(
                ".git/hooks/pre-commit",
                "#!/bin/sh\n[ \"$AUDIT_PROJECT_FLAG\" = expected ]\n",
            )
            .await;
        tokio::fs::set_permissions(
            fixture.cx.root.join(".git/hooks/pre-commit"),
            std::fs::Permissions::from_mode(0o755),
        )
        .await
        .unwrap();
        let result = tokio::spawn(async move { fixture.commit(&["mine.txt"]).await })
            .await
            .unwrap();
        assert!(!result.is_error(), "{result:?}");
    }

    #[tokio::test]
    async fn foreign_staging_is_refused_without_changing_index_or_head() {
        let fixture = Fixture::new().await;
        fixture.write("mine.txt", "old").await;
        assert!(!fixture.commit(&["mine.txt"]).await.is_error());
        fixture.write("mine.txt", "new").await;
        fixture.write("foreign.txt", "peer work").await;
        fixture.git(&["add", "--", "foreign.txt"]).await;
        let index = fixture.git(&["ls-files", "--stage", "-z"]).await;
        let head = fixture.git(&["rev-parse", "HEAD"]).await;
        let result = fixture.commit(&["mine.txt"]).await;
        assert!(result.is_error(), "{result:?}");
        assert_eq!(index, fixture.git(&["ls-files", "--stage", "-z"]).await);
        assert_eq!(head, fixture.git(&["rev-parse", "HEAD"]).await);
    }

    #[tokio::test]
    async fn final_commit_excludes_foreign_files_staged_after_preflight() {
        let fixture = Fixture::new().await;
        fixture.write("mine.txt", "old").await;
        assert!(!fixture.commit(&["mine.txt"]).await.is_error());
        fixture.write("mine.txt", "new").await;
        fixture.git(&["add", "--", "mine.txt"]).await;
        // Model a peer staging between the preflight and final Git command.
        fixture.write("foreign.txt", "peer work").await;
        fixture.git(&["add", "--", "foreign.txt"]).await;
        let foreign_index = fixture
            .git(&["ls-files", "--stage", "-z", "--", "foreign.txt"])
            .await;
        commit_only(
            &fixture.cx,
            &fixture.cx.root,
            "selected change",
            BTreeSet::from(["mine.txt"]),
            tokio::time::Instant::now() + COMMIT_TIME,
        )
        .await
        .unwrap();
        assert_eq!(
            fixture
                .git(&["diff-tree", "--no-commit-id", "--name-only", "-r", "HEAD"])
                .await,
            b"mine.txt\n"
        );
        assert_eq!(
            fixture.git(&["diff", "--cached", "--name-only"]).await,
            b"foreign.txt\n"
        );
        assert_eq!(
            foreign_index,
            fixture
                .git(&["ls-files", "--stage", "-z", "--", "foreign.txt"])
                .await
        );
    }

    #[tokio::test]
    async fn commits_explicit_new_files_and_both_kinds_of_deletions() {
        let fixture = Fixture::new().await;
        for path in ["keep.txt", "remove.txt", "staged removal.txt"] {
            fixture.write(path, "original").await;
        }
        let result = fixture
            .commit(&["keep.txt", "remove.txt", "staged removal.txt"])
            .await;
        assert!(!result.is_error(), "{result:?}");
        fixture.write("keep.txt", "changed").await;
        fixture.write("-new file.txt", "new").await;
        fixture.write("untouched.txt", "peer").await;
        for path in ["remove.txt", "staged removal.txt"] {
            tokio::fs::remove_file(fixture.cx.root.join(path))
                .await
                .unwrap();
        }
        fixture.git(&["add", "--", "staged removal.txt"]).await;
        let result = fixture
            .commit(&[
                "keep.txt",
                "remove.txt",
                "staged removal.txt",
                "-new file.txt",
            ])
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            fixture.git(&["ls-tree", "-r", "--name-only", "HEAD"]).await,
            b"-new file.txt\nkeep.txt\n"
        );
        assert_eq!(fixture.git(&["show", "HEAD:keep.txt"]).await, b"changed");
        assert!(
            fixture
                .git(&["diff", "--cached", "--name-only"])
                .await
                .is_empty()
        );
        assert_eq!(
            tokio::fs::read(fixture.cx.root.join("untouched.txt"))
                .await
                .unwrap(),
            b"peer"
        );
    }

    #[tokio::test]
    async fn rejects_broad_secret_and_missing_paths_before_staging_anything() {
        let fixture = Fixture::new().await;
        fixture.write("mine.txt", "mine").await;
        fixture.write("config/secret.pem", "secret fixture").await;
        fixture
            .write(".env.private/data.txt", "secret fixture")
            .await;
        for path in [
            ".",
            "config",
            "config/",
            "config/*",
            ":(glob)**",
            "../mine.txt",
            "./mine.txt",
            "/tmp/mine.txt",
            "config/secret.pem",
            ".env.private/data.txt",
            "missing.txt",
            "config//secret.pem",
        ] {
            let result = fixture.commit(&["mine.txt", path]).await;
            assert!(result.is_error(), "accepted {path:?}: {result:?}");
            assert!(
                fixture.git(&["ls-files", "-z"]).await.is_empty(),
                "staged files for {path:?}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rejects_symlink_ancestors_and_files() {
        let fixture = Fixture::new().await;
        fixture.write("config/secret.pem", "secret fixture").await;
        fixture.write("config/plain.txt", "plain fixture").await;
        std::os::unix::fs::symlink("config", fixture.cx.root.join("alias")).unwrap();
        std::os::unix::fs::symlink("config/secret.pem", fixture.cx.root.join("plain.txt")).unwrap();
        for path in ["alias/plain.txt", "plain.txt"] {
            assert!(fixture.commit(&[path]).await.is_error());
            assert!(fixture.git(&["ls-files", "-z"]).await.is_empty());
        }
    }
}

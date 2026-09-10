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
        "Commit the current worktree contents of explicit, literal repository-relative files (including new files and deletions). Refuses directories, globs, pathspec magic, symlinks and sensitive paths. Refuses if other files are already staged; leave other agents' staging alone and use an isolated worktree. Commits only the named files, including all their unstaged changes."
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

async fn git(cx: &ToolCx, root: &Path, args: &[&str]) -> anyhow::Result<Vec<u8>> {
    let mut command = tokio::process::Command::new("git");
    command
        .arg("--literal-pathspecs")
        .args(args)
        .current_dir(root);
    cx.child_env.apply(command.as_std_mut());
    command.envs(cx.shell_env.iter());
    // Fixture subprocesses must not read the operator's Git configuration.
    #[cfg(test)]
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    let output = command.output().await.context("start git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}{}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output.stdout)
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
    let root_output = git(cx, &effective_root, &["rev-parse", "--show-toplevel"]).await?;
    let root_text = std::str::from_utf8(&root_output).context("repository path is not UTF-8")?;
    let root = PathBuf::from(root_text.strip_suffix('\n').unwrap_or(root_text));
    let paths: BTreeSet<&str> = args.paths.iter().map(String::as_str).collect();

    if !git(cx, &root, &["ls-files", "--unmerged", "-z"])
        .await?
        .is_empty()
    {
        bail!("refused: resolve the repository's unmerged index entries before committing");
    }
    let staged = git(
        cx,
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
    let index_output = git(cx, &root, &index_args).await?;
    let index = path_modes(&index_output)?;
    // HEAD is absent in a new repository. Only skip the tree read in that case.
    let head_output = if git(cx, &root, &["rev-parse", "--verify", "HEAD"])
        .await
        .is_ok()
    {
        let mut tree_args = vec!["ls-tree", "-r", "-z", "HEAD", "--"];
        tree_args.extend(paths.iter().copied());
        git(cx, &root, &tree_args).await?
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
        git(cx, &root, &add).await?;
    }
    commit_only(cx, &root, &args.message, paths).await
}

async fn commit_only(
    cx: &ToolCx,
    root: &Path,
    message: &str,
    paths: BTreeSet<&str>,
) -> anyhow::Result<String> {
    let mut commit_args = vec!["commit", "--only", "-m", message, "--"];
    commit_args.extend(paths);
    let output = git(cx, root, &commit_args).await?;
    Ok(String::from_utf8_lossy(&output).into_owned())
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

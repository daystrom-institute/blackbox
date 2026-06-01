//! Fleet-owned git worktree lifecycle tools.
//!
//! These are intentionally narrow: they create/remove only managed worktrees
//! under a configured root and use `bro-fleet/*` branch names.

use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_BRANCH_PREFIX: &str = "bro-fleet";

#[derive(Debug, Deserialize, JsonSchema)]
struct EnterWorktreeInput {
    /// Short human-readable reason for the isolated worktree.
    purpose: String,
    /// Base ref: current (default), main, or parent_head.
    #[serde(default)]
    base: Option<String>,
    /// Optional explicit branch prefix. Must still live under bro-fleet/.
    #[serde(default)]
    branch_prefix: Option<String>,
}

pub struct EnterWorktree;

#[async_trait]
impl Tool for EnterWorktree {
    fn name(&self) -> &str {
        "enter_worktree"
    }

    fn description(&self) -> &str {
        "Create a managed isolated git worktree. Returns cwd, branch, grounding text, and env overrides. Uses BRO_FLEET_* env when present and otherwise infers the current git repository. Branches are constrained to bro-fleet/*."
    }

    fn input_schema(&self) -> Value {
        schema_for::<EnterWorktreeInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: EnterWorktreeInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(enter_worktree(&cx.root, args))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ExitWorktreeInput {
    /// Worktree path. Omit to target the current tool root.
    #[serde(default)]
    worktree: Option<String>,
    /// keep, preflight, discard, or publish. Default keep.
    #[serde(default)]
    disposition: Option<String>,
    /// Commit message for publish.
    #[serde(default)]
    commit_message: Option<String>,
    /// Explicit pathspecs to stage for publish. Empty means stage every changed
    /// path in the managed worktree, but never outside it.
    #[serde(default)]
    paths: Vec<String>,
    /// Required for publish and discard.
    #[serde(default)]
    confirm: bool,
}

pub struct ExitWorktree;

#[async_trait]
impl Tool for ExitWorktree {
    fn name(&self) -> &str {
        "exit_worktree"
    }

    fn description(&self) -> &str {
        "Finish a managed fleet worktree. disposition=keep reports status only. disposition=preflight reports the exact publish/discard readiness without mutating. disposition=discard removes a clean/confirmed managed worktree. disposition=publish commits selected changes, fetches/rebases onto origin/main, fast-forwards main, pushes main, and removes the worktree. publish/discard require confirm=true."
    }

    fn input_schema(&self) -> Value {
        schema_for::<ExitWorktreeInput>()
    }

    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            destructive: true,
            ..Default::default()
        }
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ExitWorktreeInput = match serde_json::from_value(input) {
            Ok(args) => args,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        ToolResult::from_result(exit_worktree(&cx.root, args))
    }
}

fn enter_worktree(cx_root: &Path, args: EnterWorktreeInput) -> anyhow::Result<Value> {
    let parent_worktree = git_toplevel(cx_root)?;
    let base_repo = fleet_base_repo(cx_root)?;
    let worktree_root = fleet_worktree_root(&base_repo)?;
    std::fs::create_dir_all(&worktree_root)?;
    let worktree_root = worktree_root.canonicalize()?;

    let prefix = args
        .branch_prefix
        .as_deref()
        .unwrap_or(DEFAULT_BRANCH_PREFIX)
        .trim_matches('/');
    if prefix != DEFAULT_BRANCH_PREFIX && !prefix.starts_with("bro-fleet/") {
        anyhow::bail!("branch_prefix must be bro-fleet or bro-fleet/*");
    }
    let slug = prompt_slug(&args.purpose);
    let id = short_id();
    let branch = format!("{prefix}/{slug}-{id}");
    let repo_name = base_repo
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("repo");
    let path = worktree_root
        .join(sanitize_path_component(repo_name))
        .join(format!("{slug}-{id}"));
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let base_ref = match args.base.as_deref().unwrap_or("current") {
        "current" | "parent_head" => git_capture(&parent_worktree, &["rev-parse", "HEAD"])?,
        "main" => {
            if git_ok(&base_repo, &["rev-parse", "--verify", "origin/main"]) {
                "origin/main".to_string()
            } else {
                "main".to_string()
            }
        }
        other => anyhow::bail!("base must be current, parent_head, or main; got {other}"),
    };
    git_run(
        &base_repo,
        &[
            "worktree",
            "add",
            "-b",
            &branch,
            path_str(&path)?,
            &base_ref,
        ],
    )?;
    let path = path.canonicalize()?;
    let base_branch = git_capture(&parent_worktree, &["rev-parse", "--abbrev-ref", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let base_sha = git_capture(&parent_worktree, &["rev-parse", "--short=12", "HEAD"])
        .unwrap_or_else(|_| "unknown".to_string());
    let status = git_capture(&path, &["status", "--short", "--branch"]).unwrap_or_default();
    let cargo_target = base_repo.join("target");
    let mut env = json!({
        "BRO_FLEET_BASE_REPO": base_repo.display().to_string(),
        "BRO_FLEET_WORKTREE_ROOT": worktree_root.display().to_string(),
        "BRO_FLEET_PARENT_WORKTREE": parent_worktree.display().to_string(),
        "BRO_FLEET_WORKTREE_BRANCH": branch,
    });
    if base_repo.join("Cargo.toml").is_file() {
        env["CARGO_TARGET_DIR"] = json!(cargo_target.display().to_string());
    }
    let grounding = format!(
        "[fleet worktree grounding]\n\
You are running in a managed isolated git worktree.\n\
Worktree path: {}\n\
Worktree branch: {}\n\
Base repository: {}\n\
Base branch/ref: {} @ {}\n\
Make code changes only inside this worktree unless the operator explicitly redirects you.\n\
\n\
Initial git status:\n```text\n{}\n```",
        path.display(),
        env["BRO_FLEET_WORKTREE_BRANCH"]
            .as_str()
            .unwrap_or("unknown"),
        base_repo.display(),
        base_branch.trim(),
        base_sha.trim(),
        status.trim(),
    );
    Ok(json!({
        "ok": true,
        "cwd": path,
        "branch": env["BRO_FLEET_WORKTREE_BRANCH"],
        "base_repo": base_repo,
        "worktree_root": worktree_root,
        "grounding": grounding,
        "env_overrides": env,
        "next_step": "Enter the returned cwd with the returned grounding and env_overrides.",
    }))
}

fn exit_worktree(cx_root: &Path, args: ExitWorktreeInput) -> anyhow::Result<Value> {
    let worktree = args
        .worktree
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cx_root.to_path_buf())
        .canonicalize()?;
    ensure_managed_worktree(&worktree)?;
    let base_repo = fleet_base_repo(cx_root)?;
    let branch = git_capture(&worktree, &["rev-parse", "--abbrev-ref", "HEAD"])?;
    if !branch.starts_with("bro-fleet/") {
        anyhow::bail!("refusing to exit non-fleet branch {branch}");
    }
    let disposition = args.disposition.as_deref().unwrap_or("keep");
    let status = git_capture(&worktree, &["status", "--short", "--branch"]).unwrap_or_default();
    match disposition {
        "keep" => Ok(json!({
            "ok": true,
            "disposition": "keep",
            "worktree": worktree,
            "branch": branch,
            "status": status,
        })),
        "preflight" => publish_preflight(&base_repo, &worktree, &branch, &status, &args.paths),
        "discard" => {
            if !args.confirm {
                anyhow::bail!("discard requires confirm=true");
            }
            git_run(
                &base_repo,
                &["worktree", "remove", "--force", path_str(&worktree)?],
            )?;
            let _ = git_run(&base_repo, &["branch", "-D", &branch]);
            Ok(json!({"ok": true, "disposition": "discard", "branch": branch}))
        }
        "publish" => {
            if !args.confirm {
                anyhow::bail!("publish requires confirm=true");
            }
            let message = args
                .commit_message
                .as_deref()
                .filter(|m| !m.trim().is_empty())
                .ok_or_else(|| anyhow::anyhow!("publish requires commit_message"))?;
            let changed = changed_paths(&worktree)?;
            if changed.is_empty() {
                anyhow::bail!("publish found no changed paths to commit");
            }
            ensure_base_ready_for_publish(&base_repo)?;
            git_run(&base_repo, &["fetch", "origin", "main"])?;
            git_run(&base_repo, &["merge", "--ff-only", "origin/main"])?;
            let paths = if args.paths.is_empty() {
                changed
            } else {
                args.paths
            };
            for p in &paths {
                if !is_safe_pathspec(p) {
                    anyhow::bail!("refusing unsafe pathspec {p}");
                }
            }
            let mut add_args = vec!["add".to_string(), "--".to_string()];
            add_args.extend(paths);
            git_run_owned(&worktree, &add_args)?;
            git_run(&worktree, &["commit", "-m", message])?;
            let remaining = changed_paths(&worktree)?;
            if !remaining.is_empty() {
                anyhow::bail!(
                    "publish left uncommitted changes in the worktree; refusing to remove it"
                );
            }
            git_run(&worktree, &["rebase", "main"])?;
            git_run(&base_repo, &["merge", "--ff-only", &branch])?;
            git_run(&base_repo, &["push", "origin", "main"])?;
            let head = git_capture(&base_repo, &["rev-parse", "--short=12", "HEAD"])?;
            git_run(&base_repo, &["worktree", "remove", path_str(&worktree)?])?;
            let _ = git_run(&base_repo, &["branch", "-D", &branch]);
            Ok(json!({
                "ok": true,
                "disposition": "publish",
                "published_head": head,
                "branch": branch,
                "removed_worktree": worktree,
            }))
        }
        other => {
            anyhow::bail!("disposition must be keep, preflight, discard, or publish; got {other}")
        }
    }
}

fn publish_preflight(
    base_repo: &Path,
    worktree: &Path,
    branch: &str,
    status: &str,
    requested_paths: &[String],
) -> anyhow::Result<Value> {
    let changed = changed_paths(worktree)?;
    let selected_paths = if requested_paths.is_empty() {
        changed.clone()
    } else {
        requested_paths.to_vec()
    };
    let unsafe_paths: Vec<String> = selected_paths
        .iter()
        .filter(|p| !is_safe_pathspec(p))
        .cloned()
        .collect();
    let base_branch = git_capture(base_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let base_dirty = git_capture(base_repo, &["status", "--porcelain=v1"])
        .unwrap_or_else(|e| format!("unavailable: {e}"));
    let base_ready = base_branch.trim() == "main" && base_dirty.trim().is_empty();
    let origin_main = git_capture(
        base_repo,
        &["rev-parse", "--verify", "--short=12", "origin/main"],
    )
    .ok();
    let main_head = git_capture(base_repo, &["rev-parse", "--short=12", "HEAD"]).ok();
    let main_vs_origin = git_capture(
        base_repo,
        &["rev-list", "--left-right", "--count", "HEAD...origin/main"],
    )
    .ok();

    Ok(json!({
        "ok": unsafe_paths.is_empty() && base_ready && !changed.is_empty(),
        "disposition": "preflight",
        "worktree": worktree,
        "branch": branch,
        "worktree_status": status,
        "changed_paths": changed,
        "selected_paths": selected_paths,
        "unsafe_paths": unsafe_paths,
        "base_repo": base_repo,
        "base_branch": base_branch,
        "base_dirty": base_dirty,
        "base_ready": base_ready,
        "main_head": main_head,
        "origin_main_head": origin_main,
        "main_vs_origin": main_vs_origin,
        "publish_plan": [
            "require confirm=true",
            "ensure base repo is clean and on main",
            "git fetch origin main",
            "git merge --ff-only origin/main in base repo",
            "git add -- selected paths in managed worktree",
            "git commit in managed worktree",
            "git rebase main in managed worktree",
            "git merge --ff-only branch into main",
            "git push origin main",
            "git worktree remove and delete bro-fleet branch"
        ],
    }))
}

fn fleet_base_repo(cx_root: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(raw) = std::env::var("BRO_FLEET_BASE_REPO")
        && !raw.trim().is_empty()
    {
        return Ok(PathBuf::from(raw).canonicalize()?);
    }
    primary_worktree(cx_root)
}

fn fleet_worktree_root(anchor: &Path) -> anyhow::Result<PathBuf> {
    if let Ok(raw) = std::env::var("BRO_FLEET_WORKTREE_ROOT")
        && !raw.trim().is_empty()
    {
        return Ok(PathBuf::from(raw));
    }
    let repo = git_toplevel(anchor)?;
    let repo_name = repo.file_name().and_then(|n| n.to_str()).unwrap_or("repo");
    let parent = repo.parent().unwrap_or(&repo);
    Ok(parent
        .join(".bro-fleet-worktrees")
        .join(sanitize_path_component(repo_name)))
}

fn ensure_managed_worktree(path: &Path) -> anyhow::Result<()> {
    let root = if let Ok(raw) = std::env::var("BRO_FLEET_WORKTREE_ROOT")
        && !raw.trim().is_empty()
    {
        PathBuf::from(raw).canonicalize()?
    } else {
        let base = fleet_base_repo(path)?;
        fleet_worktree_root(&base)?.canonicalize()?
    };
    if !path.starts_with(&root) {
        anyhow::bail!(
            "refusing unmanaged worktree {}; expected under {}",
            path.display(),
            root.display()
        );
    }
    Ok(())
}

fn ensure_base_ready_for_publish(base_repo: &Path) -> anyhow::Result<()> {
    let branch = git_capture(base_repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])?;
    if branch.trim() != "main" {
        anyhow::bail!("publish requires base repository to be on main; currently {branch}");
    }
    let dirty = git_capture(base_repo, &["status", "--porcelain=v1"])?;
    if !dirty.trim().is_empty() {
        anyhow::bail!("publish requires a clean base repository; dirty status:\n{dirty}");
    }
    Ok(())
}

fn changed_paths(repo: &Path) -> anyhow::Result<Vec<String>> {
    let raw = git_capture(repo, &["status", "--porcelain=v1"])?;
    Ok(raw
        .lines()
        .filter_map(|line| line.get(3..).map(str::trim))
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches('"').to_string())
        .collect())
}

fn git_toplevel(cwd: &Path) -> anyhow::Result<PathBuf> {
    Ok(PathBuf::from(git_capture(cwd, &["rev-parse", "--show-toplevel"])?).canonicalize()?)
}

fn primary_worktree(cwd: &Path) -> anyhow::Result<PathBuf> {
    let raw = git_capture(cwd, &["worktree", "list", "--porcelain"])?;
    for line in raw.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            return Ok(PathBuf::from(path).canonicalize()?);
        }
    }
    git_toplevel(cwd)
}

fn is_safe_pathspec(raw: &str) -> bool {
    let path = Path::new(raw);
    !path.is_absolute()
        && path.components().all(|component| {
            !matches!(
                component,
                std::path::Component::ParentDir | std::path::Component::Prefix(_)
            )
        })
}

fn git_ok(cwd: &Path, args: &[&str]) -> bool {
    Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .is_ok_and(|o| o.status.success())
}

fn git_capture(cwd: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

fn git_run(cwd: &Path, args: &[&str]) -> anyhow::Result<()> {
    let out = Command::new("git").arg("-C").arg(cwd).args(args).output()?;
    if out.status.success() {
        Ok(())
    } else {
        anyhow::bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
}

fn git_run_owned(cwd: &Path, args: &[String]) -> anyhow::Result<()> {
    let borrowed = args.iter().map(String::as_str).collect::<Vec<_>>();
    git_run(cwd, &borrowed)
}

fn path_str(path: &Path) -> anyhow::Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 path {}", path.display()))
}

fn prompt_slug(prompt: &str) -> String {
    let slug = sanitize_path_component(prompt)
        .trim_matches('-')
        .chars()
        .take(36)
        .collect::<String>();
    if slug.is_empty() {
        "task".to_string()
    } else {
        slug
    }
}

fn sanitize_path_component(raw: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in raw.chars() {
        let normalized = if ch.is_ascii_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if normalized == '-' {
            if !last_dash {
                out.push('-');
                last_dash = true;
            }
        } else {
            out.push(normalized);
            last_dash = false;
        }
    }
    out.trim_matches('-').to_string()
}

fn short_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:x}", nanos).chars().rev().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool::ToolCx;
    use std::sync::{Arc, Mutex};

    fn cx(root: &Path) -> ToolCx {
        ToolCx {
            root: root.to_path_buf(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(crate::shell::ShellSessions::default())),
            promises: Arc::new(Mutex::new(crate::promise::PromiseStore::default())),
            clipboard: Arc::new(Mutex::new(crate::clipboard::Registers::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
        }
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("git")
            .arg("-C")
            .arg(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn seed_repo() -> tempfile::TempDir {
        let repo = tempfile::tempdir().unwrap();
        run_git(repo.path(), &["init", "-b", "main"]);
        run_git(repo.path(), &["config", "user.email", "test@example.com"]);
        run_git(repo.path(), &["config", "user.name", "Test User"]);
        std::fs::write(repo.path().join("README.md"), "base\n").unwrap();
        run_git(repo.path(), &["add", "."]);
        run_git(repo.path(), &["commit", "-m", "init"]);
        repo
    }

    async fn enter_test_worktree(repo: &Path) -> Value {
        let tool = EnterWorktree;
        let result = tool
            .call(json!({"purpose":"isolated task"}), &cx(repo))
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        serde_json::from_str(&content).unwrap()
    }

    #[tokio::test]
    async fn enter_creates_managed_worktree() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        assert!(cwd.join("README.md").is_file());
        assert!(
            value["branch"]
                .as_str()
                .unwrap()
                .starts_with("bro-fleet/isolated-task-")
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_reports_publish_plan_without_mutating() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\nchange\n").unwrap();

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight",
                    "paths": ["README.md"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["disposition"], "preflight");
        assert_eq!(report["ok"], true);
        assert_eq!(report["changed_paths"], json!(["README.md"]));
        assert_eq!(report["selected_paths"], json!(["README.md"]));
        assert!(report["publish_plan"].as_array().unwrap().len() >= 5);
        assert!(cwd.join("README.md").is_file());
        assert_eq!(
            git_capture(&cwd, &["rev-list", "--count", "HEAD"]).unwrap(),
            "1"
        );

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }

    #[tokio::test]
    async fn exit_preflight_reports_unsafe_pathspecs() {
        let repo = seed_repo();
        let value = enter_test_worktree(repo.path()).await;
        let cwd = PathBuf::from(value["cwd"].as_str().unwrap());
        std::fs::write(cwd.join("README.md"), "base\nchange\n").unwrap();

        let tool = ExitWorktree;
        let result = tool
            .call(
                json!({
                    "worktree": cwd,
                    "disposition": "preflight",
                    "paths": ["../outside"]
                }),
                &cx(&cwd),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        let report: Value = serde_json::from_str(&content).unwrap();

        assert_eq!(report["ok"], false);
        assert_eq!(report["unsafe_paths"], json!(["../outside"]));

        run_git(
            repo.path(),
            &["worktree", "remove", "--force", cwd.to_str().unwrap()],
        );
        std::fs::remove_dir_all(PathBuf::from(value["worktree_root"].as_str().unwrap())).ok();
    }
}

use super::{ArcContext, OpEffect};
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;
use std::process::Stdio;

pub(super) async fn exec_worktree_create(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.path"))?
        .to_string();
    let branch = args
        .get("branch")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeCreate requires args.branch"))?
        .to_string();
    let base = args
        .get("base")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let repo_root = ctx
        .meta
        .project_dir
        .clone()
        .ok_or_else(|| anyhow!("WorktreeCreate: meta.project_dir not set"))?;

    if Path::new(&path).exists() {
        bail!("WorktreeCreate: path {path} already exists");
    }

    let branch_status = branch_state(&repo_root, &branch).await?;
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(&repo_root).arg("worktree").arg("add");
    match branch_status {
        BranchState::AbsentLocal => {
            cmd.arg("-b").arg(&branch).arg(&path).arg(&base);
        }
        BranchState::PresentFree => {
            cmd.arg(&path).arg(&branch);
        }
        BranchState::PresentInUse(other_path) => {
            bail!(
                "WorktreeCreate: branch '{branch}' is already checked out at {other_path} \
                 (concurrent arc on same issue?). Either let the other arc finish, or \
                 retire its worktree, or use a unique branch name (e.g. include \
                 ${{meta.arc_id}} in the name)."
            );
        }
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree add spawn: {e}"))?;
    if !output.status.success() {
        bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    // Opt-in build-cache seeding (fleet.json project_dispatch.seed_dirs):
    // CoW-clone warm dirs from the base repo so the arc's first build is
    // incremental instead of cold. Best-effort and off the async runtime —
    // the clone is pure blocking fs work.
    let seed_dirs = bro_fleet_client::FleetConfig::load()
        .project_dispatch_for(Path::new(&repo_root))
        .map(|d| d.seed_dirs.clone())
        .unwrap_or_default();
    if !seed_dirs.is_empty() {
        let base = std::path::PathBuf::from(&repo_root);
        let wt = std::path::PathBuf::from(&path);
        let outcomes = tokio::task::spawn_blocking(move || {
            bro_fleet_client::seed_worktree_dirs(&base, &wt, &seed_dirs)
        })
        .await
        .unwrap_or_else(|e| vec![format!("seed: join error: {e}")]);
        for outcome in outcomes {
            tracing::info!(worktree = %path, "{outcome}");
        }
    }
    Ok(OpEffect::SetWorktree(Some(path)))
}

enum BranchState {
    AbsentLocal,
    PresentFree,
    PresentInUse(String),
}

async fn branch_state(repo_root: &str, branch: &str) -> Result<BranchState> {
    let exists = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("show-ref")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("refs/heads/{branch}"))
        .status()
        .await
        .map_err(|e| anyhow!("git show-ref spawn: {e}"))?
        .success();
    if !exists {
        return Ok(BranchState::AbsentLocal);
    }
    let porcelain = tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo_root)
        .arg("worktree")
        .arg("list")
        .arg("--porcelain")
        .output()
        .await
        .map_err(|e| anyhow!("git worktree list spawn: {e}"))?;
    if !porcelain.status.success() {
        bail!(
            "git worktree list failed: {}",
            String::from_utf8_lossy(&porcelain.stderr)
        );
    }
    let text = String::from_utf8_lossy(&porcelain.stdout).into_owned();
    let needle = format!("branch refs/heads/{branch}");
    let mut current_path: Option<String> = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            current_path = Some(p.to_string());
            continue;
        }
        if line.trim() == needle {
            if let Some(p) = current_path.take() {
                return Ok(BranchState::PresentInUse(p));
            }
        }
        if line.trim().is_empty() {
            current_path = None;
        }
    }
    Ok(BranchState::PresentFree)
}

pub(super) async fn exec_worktree_remove(args: &Value, ctx: &ArcContext) -> Result<OpEffect> {
    let path = args
        .get("path")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("WorktreeRemove requires args.path"))?
        .to_string();
    let force = args.get("force").and_then(|v| v.as_bool()).unwrap_or(false);

    if !Path::new(&path).exists() {
        return Ok(OpEffect::SetWorktree(None));
    }

    let repo_root = args
        .get("repo_root")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| ctx.meta.project_dir.clone())
        .ok_or_else(|| {
            anyhow!(
                "WorktreeRemove: no repo_root resolvable (set args.repo_root or meta.project_dir)"
            )
        })?;

    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C")
        .arg(&repo_root)
        .arg("worktree")
        .arg("remove")
        .arg(&path);
    if force {
        cmd.arg("--force");
    }
    let output = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| anyhow!("git worktree remove spawn: {e}"))?;
    if !output.status.success() {
        bail!(
            "git worktree remove failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(OpEffect::SetWorktree(None))
}

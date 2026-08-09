//! Worker-local managed-workspace inspection.
//!
//! The daemon supplies a bounded set of catalog-approved durable scopes.
//! fleetd verifies only local filesystem and committed-Git facts, then returns
//! the exact matching scope plus the reuse-safe checkout marker. It never
//! reads daemon policy, a brofile, or a project catalog.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use bro_core::WorkspaceId;
use bro_protocol::{
    WorkerWorkspaceIdentity, WorkerWorkspaceScope, WorkspaceInspectionOutcome,
    WorkspaceInspectionRequest,
};
use tokio::io::AsyncReadExt;
use tokio::process::Command;

const MANAGED_CHECKOUT_MARKER: &str = "blackbox-managed-checkout-v1";
const MAX_CANDIDATE_SCOPES: usize = 4096;
const MAX_CWD_BYTES: usize = 16 * 1024;
const MAX_GIT_PATH_OUTPUT_BYTES: usize = 64 * 1024;
const MAX_PROJECT_CONFIG_BYTES: usize = 1024 * 1024;

/// Inspect one cwd without widening the daemon-supplied authority set.
pub async fn inspect_workspace(request: WorkspaceInspectionRequest) -> WorkspaceInspectionOutcome {
    match inspect_workspace_inner(request).await {
        Ok(outcome) => outcome,
        Err(error) => WorkspaceInspectionOutcome::Refused {
            code: "workspace.inspect_failed".to_string(),
            message: format!("worker-local workspace inspection failed: {error}"),
        },
    }
}

async fn inspect_workspace_inner(
    request: WorkspaceInspectionRequest,
) -> Result<WorkspaceInspectionOutcome> {
    if request.cwd.len() > MAX_CWD_BYTES {
        bail!("cwd exceeds the inspection bound");
    }
    if request.candidate_scopes.len() > MAX_CANDIDATE_SCOPES {
        bail!("candidate scope count exceeds the inspection bound");
    }
    let cwd = Path::new(&request.cwd);
    if !cwd.is_absolute() {
        bail!("cwd is not absolute");
    }
    let cwd = tokio::fs::canonicalize(cwd)
        .await
        .context("canonicalizing cwd")?;
    if !tokio::fs::metadata(&cwd).await?.is_dir() {
        bail!("cwd is not a directory");
    }

    let root_output = git_output(
        &cwd,
        &["rev-parse", "--show-toplevel"],
        MAX_GIT_PATH_OUTPUT_BYTES,
    )
    .await;
    let Ok(root_output) = root_output else {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    };
    let root_text = std::str::from_utf8(&root_output).context("Git root is not UTF-8")?;
    let root = tokio::fs::canonicalize(root_text.trim())
        .await
        .context("canonicalizing Git root")?;
    if !cwd.starts_with(&root) {
        bail!("Git root does not contain cwd");
    }

    let dot_git = root.join(".git");
    let Ok(dot_git_metadata) = tokio::fs::symlink_metadata(&dot_git).await else {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    };
    if !dot_git_metadata.file_type().is_dir() {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    }
    let marker_path = dot_git.join("blackbox-managed-checkout");
    let Ok(marker_metadata) = tokio::fs::symlink_metadata(&marker_path).await else {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    };
    if !marker_metadata.file_type().is_file() || marker_metadata.len() > 128 {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    }
    let marker = tokio::fs::read_to_string(&marker_path).await?;
    if marker.trim() != MANAGED_CHECKOUT_MARKER {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    }

    let candidate_scopes = request
        .candidate_scopes
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut matches = Vec::new();
    for scope in candidate_scopes {
        let project_root = project_root_for_scope(&root, &scope);
        let Ok(project_root) = tokio::fs::canonicalize(project_root).await else {
            continue;
        };
        if !project_root.starts_with(&root) || !cwd.starts_with(&project_root) {
            continue;
        }
        let config_relpath = if scope.bbox_root_relpath() == "." {
            ".bbox/config.toml".to_string()
        } else {
            format!("{}/.bbox/config.toml", scope.bbox_root_relpath())
        };
        let config = match git_output(
            &root,
            &["show", &format!("HEAD:{config_relpath}")],
            MAX_PROJECT_CONFIG_BYTES,
        )
        .await
        {
            Ok(config) => config,
            Err(_) => continue,
        };
        let config = match std::str::from_utf8(&config) {
            Ok(config) => config,
            Err(_) => continue,
        };
        let parsed = match config.parse::<toml::Value>() {
            Ok(parsed) => parsed,
            Err(_) => continue,
        };
        let recorded_repo_id = parsed
            .get("project")
            .and_then(|project| project.get("repo_id"))
            .and_then(toml::Value::as_str);
        if recorded_repo_id != Some(scope.repo_id()) {
            continue;
        }
        matches.push((project_root.components().count(), scope));
    }

    matches.sort_by(|left, right| right.0.cmp(&left.0));
    let Some((depth, scope)) = matches.first().cloned() else {
        return Ok(WorkspaceInspectionOutcome::Refused {
            code: "workspace.scope_unrecognized".to_string(),
            message: "managed checkout does not prove a daemon-authorized project scope"
                .to_string(),
        });
    };
    if matches
        .iter()
        .skip(1)
        .any(|(candidate_depth, _)| *candidate_depth == depth)
    {
        return Ok(WorkspaceInspectionOutcome::Refused {
            code: "workspace.scope_ambiguous".to_string(),
            message: "managed checkout matches more than one equally specific project scope"
                .to_string(),
        });
    }

    let workspace_id = tokio::task::spawn_blocking({
        let root = root.clone();
        move || ensure_workspace_id(&root)
    })
    .await
    .context("joining workspace identity creation")??;
    Ok(WorkspaceInspectionOutcome::Managed {
        identity: WorkerWorkspaceIdentity {
            workspace_id,
            scope,
        },
    })
}

fn project_root_for_scope(root: &Path, scope: &WorkerWorkspaceScope) -> PathBuf {
    if scope.bbox_root_relpath() == "." {
        root.to_path_buf()
    } else {
        root.join(scope.bbox_root_relpath())
    }
}

async fn git_output(cwd: &Path, args: &[&str], max_bytes: usize) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("starting Git inspection")?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("Git inspection has no stdout"))?;
    let mut bytes = Vec::new();
    stdout
        .take((max_bytes + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .context("reading Git inspection output")?;
    if bytes.len() > max_bytes {
        let _ = child.kill().await;
        let _ = child.wait().await;
        bail!("Git inspection output exceeds its bound");
    }
    let status = child.wait().await.context("waiting for Git inspection")?;
    if !status.success() {
        bail!("Git inspection command failed");
    }
    Ok(bytes)
}

#[allow(clippy::disallowed_methods)] // worker-local create-once identity marker
fn ensure_workspace_id(root: &Path) -> Result<WorkspaceId> {
    let bbox_dir = ensure_real_directory(&root.join(".bbox"))?;
    let local_dir = ensure_real_directory(&bbox_dir.join("local"))?;
    let marker = local_dir.join("checkout-id");
    if let Some(existing) = read_workspace_id(&marker)? {
        return Ok(existing);
    }

    let minted = WorkspaceId::parse(uuid::Uuid::new_v4().simple().to_string())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    match options.open(&marker) {
        Ok(mut file) => {
            use std::io::Write as _;
            file.write_all(minted.as_str().as_bytes())?;
            file.write_all(b"\n")?;
            file.sync_all()?;
            Ok(minted)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_workspace_id(&marker)?
                .ok_or_else(|| anyhow::anyhow!("concurrent workspace identity creation vanished"))
        }
        Err(error) => Err(error).context("creating workspace identity"),
    }
}

#[allow(clippy::disallowed_methods)] // worker-local marker directory
fn ensure_real_directory(path: &Path) -> Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(path.to_path_buf()),
        Ok(_) => bail!("workspace identity parent is not a real directory"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::create_dir(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error).context("creating workspace identity parent"),
            }
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.file_type().is_dir() {
                bail!("workspace identity parent became unsafe");
            }
            Ok(path.to_path_buf())
        }
        Err(error) => Err(error).context("inspecting workspace identity parent"),
    }
}

#[allow(clippy::disallowed_methods)] // worker-local marker read with nofollow
fn read_workspace_id(path: &Path) -> Result<Option<WorkspaceId>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("opening workspace identity"),
    };
    let metadata = file.metadata()?;
    if !metadata.file_type().is_file() || metadata.len() > 128 {
        bail!("workspace identity marker is not a bounded regular file");
    }
    let mut value = String::new();
    use std::io::Read as _;
    file.take(129).read_to_string(&mut value)?;
    Ok(Some(WorkspaceId::parse(value.trim().to_string())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_once_workspace_identity_is_stable() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let first = ensure_workspace_id(&root).unwrap();
        let second = ensure_workspace_id(&root).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::read_to_string(root.join(".bbox/local/checkout-id"))
                .unwrap()
                .trim(),
            first.as_str()
        );
    }

    #[cfg(unix)]
    #[test]
    fn workspace_identity_refuses_symlinked_local_directory() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".bbox")).unwrap();
        std::fs::create_dir(root.join("elsewhere")).unwrap();
        symlink(root.join("elsewhere"), root.join(".bbox/local")).unwrap();
        assert!(ensure_workspace_id(&root).is_err());
    }
}

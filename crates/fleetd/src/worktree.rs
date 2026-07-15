use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use bro_capabilities::{ExecutionAccepted, ExecutionRequest, WorkingSetIntent};
use fleet_core::{WorktreeOwnershipRecord, WorktreeOwnershipState};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::process::Command;

use crate::authority_actor::AuthorityActor;
use crate::{FleetdError, FleetdResult};

#[derive(Debug, Clone)]
pub(crate) struct PreparedWorkingSet {
    pub request: ExecutionRequest,
    pub claim: Option<WorktreeOwnershipRecord>,
}

#[derive(Clone)]
pub(crate) struct WorktreeManager {
    root: PathBuf,
    fleet_config_path: Option<PathBuf>,
    authority: AuthorityActor,
}

impl WorktreeManager {
    pub(crate) fn new(
        root: PathBuf,
        fleet_config_path: Option<PathBuf>,
        authority: AuthorityActor,
    ) -> Self {
        Self {
            root,
            fleet_config_path,
            authority,
        }
    }

    pub(crate) fn managed_root(&self) -> &Path {
        &self.root
    }

    pub(crate) async fn prepare(
        &self,
        accepted: &ExecutionAccepted,
        mut request: ExecutionRequest,
    ) -> FleetdResult<PreparedWorkingSet> {
        let WorkingSetIntent::CreateManagedWorktree {
            base_repo,
            requested_path,
            seed_dirs,
        } = request.working_set.clone()
        else {
            return Ok(PreparedWorkingSet {
                request,
                claim: None,
            });
        };

        let base_repo = canonical_git_root(Path::new(&base_repo)).await?;
        let root = prepare_managed_root(&self.root).await?;
        let path = resolve_managed_path(
            &root,
            requested_path.as_deref(),
            &base_repo,
            accepted.task_id.as_str(),
        )?;
        let project = load_project_dispatch(self.fleet_config_path.as_deref(), &base_repo).await;
        for (key, value) in project.env {
            if looks_secret(&key) {
                tracing::warn!(
                    key,
                    "refusing secret-shaped project dispatch environment key"
                );
                continue;
            }
            request.shell_env.entry(key).or_insert(value);
        }
        let seed_dirs = merge_seed_dirs(seed_dirs, project.seed_dirs)?;
        let path_string = path.to_string_lossy().into_owned();
        let base_string = base_repo.to_string_lossy().into_owned();
        let task_id = accepted.task_id.clone();
        let attempt_id = Some(accepted.attempt_id.clone());
        let seed_for_claim = seed_dirs.clone();
        let claim = self
            .authority
            .call(move |authority| {
                authority
                    .claim_worktree(
                        path_string,
                        base_string,
                        &task_id,
                        attempt_id,
                        seed_for_claim,
                        now_ms(),
                    )
                    .map_err(Into::into)
            })
            .await?;
        self.materialize(&claim).await?;
        request.working_set = WorkingSetIntent::CreateManagedWorktree {
            base_repo: claim.base_repo.clone(),
            requested_path: Some(claim.path.clone()),
            seed_dirs,
        };
        Ok(PreparedWorkingSet {
            request,
            claim: Some(claim),
        })
    }

    pub(crate) async fn reconcile(&self) -> FleetdResult<()> {
        let snapshot = self
            .authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await?;
        for record in snapshot.worktrees.values().cloned() {
            match record.state {
                WorktreeOwnershipState::Released => {}
                WorktreeOwnershipState::Closing => {
                    if path_is_missing(Path::new(&record.path)).await? {
                        self.release_missing(&record).await?;
                    } else {
                        self.cleanup(&record).await?;
                    }
                }
                WorktreeOwnershipState::Active => {
                    let terminal = snapshot
                        .tasks
                        .get(&record.task_id)
                        .is_none_or(|task| task.status.is_terminal());
                    if terminal {
                        self.cleanup(&record).await?;
                    } else if record.materialized {
                        if path_is_missing(Path::new(&record.path)).await? {
                            // A closeout can remove the git worktree before its
                            // fenced ownership release is persisted. Treat the
                            // missing path as a recoverable completion, not a
                            // reason to terminate the reconciliation loop.
                            self.release_missing(&record).await?;
                        } else if let Err(error) = verify_materialized_worktree(
                            Path::new(&record.base_repo),
                            Path::new(&record.path),
                            &self.root,
                        )
                        .await
                        {
                            // A concurrent closeout can remove the git worktree
                            // between the path_is_missing pre-check above and this
                            // verification, so the git commands fail against a
                            // path that vanished underneath us. Re-check the path:
                            // if it is now gone, treat it as the same recoverable
                            // completion as the pre-check case rather than
                            // surfacing a hard error that would kill the loop.
                            if path_is_missing(Path::new(&record.path)).await? {
                                self.release_missing(&record).await?;
                            } else {
                                return Err(error);
                            }
                        }
                    } else {
                        self.materialize(&record).await?;
                    }
                }
            }
        }
        Ok(())
    }

    async fn materialize(&self, record: &WorktreeOwnershipRecord) -> FleetdResult<()> {
        ensure_record_is_scoped(record, &self.root).await?;
        let base_repo = PathBuf::from(&record.base_repo);
        let path = PathBuf::from(&record.path);
        match tokio::fs::symlink_metadata(&path).await {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FleetdError::InvalidRequest(format!(
                        "managed worktree {} is not a real directory",
                        path.display()
                    )));
                }
                verify_materialized_worktree(&base_repo, &path, &self.root).await?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure_real_ancestor(&path, &self.root)?;
                let output = Command::new("git")
                    .arg("-C")
                    .arg(&base_repo)
                    .args(["worktree", "add", "--detach"])
                    .arg(&path)
                    .arg("HEAD")
                    .stdin(Stdio::null())
                    .output()
                    .await
                    .map_err(|error| {
                        FleetdError::WorkerLaunch(format!("git worktree add: {error}"))
                    })?;
                if !output.status.success() {
                    return Err(FleetdError::WorkerLaunch(format!(
                        "git worktree add failed: {}",
                        bounded_stderr(&output.stderr)
                    )));
                }
                verify_materialized_worktree(&base_repo, &path, &self.root).await?;
            }
            Err(error) => return Err(error.into()),
        }

        for outcome in seed_worktree_dirs(&base_repo, &path, &record.seed_dirs).await {
            tracing::info!(worktree = %path.display(), "{outcome}");
        }
        let path_string = record.path.clone();
        let owner = record.task_id.clone();
        let generation = record.ownership_generation;
        self.authority
            .call(move |authority| {
                authority
                    .mark_worktree_materialized(&path_string, &owner, generation, now_ms())
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
    }

    pub(crate) async fn cleanup(&self, record: &WorktreeOwnershipRecord) -> FleetdResult<()> {
        ensure_record_is_scoped(record, &self.root).await?;
        let path = record.path.clone();
        let owner = record.task_id.clone();
        let generation = record.ownership_generation;
        let closing = self
            .authority
            .call(move |authority| {
                authority
                    .mark_worktree_closing(&path, &owner, generation, now_ms())
                    .map_err(Into::into)
            })
            .await?;

        if path_is_missing(Path::new(&closing.path)).await? {
            return self.release_missing(&closing).await;
        }

        run_pre_remove_hooks(self.fleet_config_path.as_deref(), &closing).await?;
        remove_worktree(&closing, &self.root).await?;
        self.release_missing(&closing).await
    }

    /// Reconcile a successful phased closeout with fleet's fenced worktree
    /// ownership. The closeout driver already ran the configured hook seam and
    /// removed the git worktree, so this path must never execute hooks again.
    pub(crate) async fn finalize_removed(&self, path: &Path) -> FleetdResult<bool> {
        let path_string = path.to_string_lossy().into_owned();
        let snapshot = self
            .authority
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await?;
        let Some(record) = snapshot.worktrees.get(&path_string).cloned() else {
            // Legacy managed worktrees may predate fleetd authority. The driver
            // still owns their safe closeout, but there is no fleet record to
            // release.
            return Ok(false);
        };
        if record.state == WorktreeOwnershipState::Released {
            return Ok(true);
        }
        if !path_is_missing(path).await? {
            return Err(FleetdError::Conflict(format!(
                "closeout reported removal but managed worktree {} still exists",
                path.display()
            )));
        }
        self.release_missing(&record).await?;
        Ok(true)
    }

    async fn release_missing(&self, record: &WorktreeOwnershipRecord) -> FleetdResult<()> {
        ensure_record_is_scoped(record, &self.root).await?;
        let path = record.path.clone();
        let owner = record.task_id.clone();
        let generation = record.ownership_generation;
        let closing = self
            .authority
            .call(move |authority| {
                authority
                    .mark_worktree_closing(&path, &owner, generation, now_ms())
                    .map_err(Into::into)
            })
            .await?;
        let path = closing.path.clone();
        let owner = closing.task_id.clone();
        let generation = closing.ownership_generation;
        self.authority
            .call(move |authority| {
                authority
                    .release_worktree(&path, &owner, generation, now_ms())
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
    }
}

async fn path_is_missing(path: &Path) -> FleetdResult<bool> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error.into()),
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct FleetProjects {
    #[serde(default)]
    project_dispatch: BTreeMap<String, ProjectDispatch>,
    #[serde(default)]
    project_closeout: BTreeMap<String, ProjectCloseout>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ProjectDispatch {
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    seed_dirs: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectCloseout {
    #[serde(default)]
    target: Option<String>,
    #[serde(default)]
    allow_branch_prefixes: Option<Vec<String>>,
    #[serde(default)]
    closeout_hooks: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    hook_policy: HookPolicy,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct HookPolicy {
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    on_fail: HookOnFail,
    #[serde(default = "default_hook_timeout_secs")]
    timeout_secs: u64,
}

impl Default for HookPolicy {
    fn default() -> Self {
        Self {
            cwd: None,
            on_fail: HookOnFail::Warn,
            timeout_secs: default_hook_timeout_secs(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum HookOnFail {
    #[default]
    Warn,
    Block,
}

fn default_hook_timeout_secs() -> u64 {
    600
}

async fn load_project_dispatch(path: Option<&Path>, base_repo: &Path) -> ProjectDispatch {
    let Some(path) = path else {
        return ProjectDispatch::default();
    };
    let config = match tokio::fs::read(path)
        .await
        .ok()
        .and_then(|body| serde_json::from_slice::<FleetProjects>(&body).ok())
    {
        Some(config) => config,
        None => return ProjectDispatch::default(),
    };
    lookup_project(&config.project_dispatch, base_repo)
        .cloned()
        .unwrap_or_default()
}

async fn load_closeout_strict(path: Option<&Path>) -> FleetdResult<FleetProjects> {
    let Some(path) = path else {
        return Ok(FleetProjects::default());
    };
    match tokio::fs::read(path).await {
        Ok(body) => serde_json::from_slice(&body).map_err(Into::into),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(FleetProjects::default()),
        Err(error) => Err(error.into()),
    }
}

fn lookup_project<'a, T>(map: &'a BTreeMap<String, T>, repo: &Path) -> Option<&'a T> {
    map.get(repo.to_string_lossy().as_ref())
}

async fn run_pre_remove_hooks(
    config_path: Option<&Path>,
    record: &WorktreeOwnershipRecord,
) -> FleetdResult<()> {
    let config = load_closeout_strict(config_path).await?;
    let Some(closeout) = lookup_project(&config.project_closeout, Path::new(&record.base_repo))
    else {
        return Ok(());
    };
    let _ = (&closeout.target, &closeout.allow_branch_prefixes);
    let Some(hooks) = closeout.closeout_hooks.get("pre_remove") else {
        return Ok(());
    };
    let cwd = closeout
        .hook_policy
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&record.base_repo));
    if !cwd.is_dir() {
        return Err(FleetdError::InvalidRequest(format!(
            "closeout hook cwd {} is not a directory",
            cwd.display()
        )));
    }
    for script in hooks {
        let mut command = Command::new("sh");
        command
            .arg("-lc")
            .arg(script)
            .current_dir(&cwd)
            .env_clear()
            .envs(inherited_process_basics())
            .env("BBOX_WORKTREE", &record.path)
            .env("BBOX_BASE_REPO", &record.base_repo)
            .env("BBOX_TARGET_DIR", Path::new(&record.path).join("target"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let outcome = tokio::time::timeout(
            Duration::from_secs(closeout.hook_policy.timeout_secs.max(1)),
            command.output(),
        )
        .await;
        let failed = match outcome {
            Ok(Ok(output)) if output.status.success() => false,
            Ok(Ok(output)) => {
                tracing::warn!(
                    script,
                    stderr = %bounded_stderr(&output.stderr),
                    "fleet worktree pre-remove hook failed"
                );
                true
            }
            Ok(Err(error)) => {
                tracing::warn!(script, %error, "fleet worktree pre-remove hook failed to start");
                true
            }
            Err(_) => {
                tracing::warn!(script, "fleet worktree pre-remove hook timed out");
                true
            }
        };
        if failed && closeout.hook_policy.on_fail == HookOnFail::Block {
            return Err(FleetdError::Conflict(
                "blocking pre-remove hook refused managed worktree cleanup".into(),
            ));
        }
    }
    Ok(())
}

async fn canonical_git_root(path: &Path) -> FleetdResult<PathBuf> {
    let canonical = tokio::fs::canonicalize(path).await.map_err(|error| {
        FleetdError::InvalidRequest(format!(
            "base repository {} cannot be canonicalized: {error}",
            path.display()
        ))
    })?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&canonical)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await
        .map_err(|error| FleetdError::InvalidRequest(format!("git rev-parse: {error}")))?;
    if !output.status.success() {
        return Err(FleetdError::InvalidRequest(format!(
            "base repository {} is not a git worktree: {}",
            canonical.display(),
            bounded_stderr(&output.stderr)
        )));
    }
    let discovered = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    tokio::fs::canonicalize(discovered)
        .await
        .map_err(Into::into)
}

async fn prepare_managed_root(root: &Path) -> FleetdResult<PathBuf> {
    tokio::fs::create_dir_all(root).await?;
    let metadata = tokio::fs::symlink_metadata(root).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "managed worktree root {} is not a real directory",
            root.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700)).await?;
    }
    Ok(tokio::fs::canonicalize(root).await?)
}

fn resolve_managed_path(
    root: &Path,
    requested: Option<&str>,
    base_repo: &Path,
    task_id: &str,
) -> FleetdResult<PathBuf> {
    let candidate = match requested {
        Some(requested) => {
            let requested = Path::new(requested);
            if requested.is_absolute() {
                requested.to_path_buf()
            } else {
                root.join(requested)
            }
        }
        None => {
            let repo = base_repo
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or("repo")
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                        character
                    } else {
                        '-'
                    }
                })
                .collect::<String>();
            let suffix = hex::encode(Sha256::digest(task_id.as_bytes()));
            root.join(format!("{repo}-{}", &suffix[..16]))
        }
    };
    if candidate
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        || !candidate.starts_with(root)
        || candidate == root
    {
        return Err(FleetdError::InvalidRequest(format!(
            "managed worktree path {} escapes {}",
            candidate.display(),
            root.display()
        )));
    }
    ensure_real_ancestor(&candidate, root)?;
    Ok(candidate)
}

fn ensure_real_ancestor(path: &Path, root: &Path) -> FleetdResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| FleetdError::InvalidRequest("managed worktree path has no parent".into()))?;
    let mut cursor = parent;
    loop {
        match std::fs::symlink_metadata(cursor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FleetdError::InvalidRequest(format!(
                        "managed worktree ancestor {} is not a real directory",
                        cursor.display()
                    )));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if cursor == root {
            return Ok(());
        }
        cursor = cursor.parent().ok_or_else(|| {
            FleetdError::InvalidRequest(format!(
                "managed worktree path {} escapes its root",
                path.display()
            ))
        })?;
    }
}

async fn ensure_record_is_scoped(
    record: &WorktreeOwnershipRecord,
    root: &Path,
) -> FleetdResult<()> {
    let root = tokio::fs::canonicalize(root).await?;
    let path = Path::new(&record.path);
    if !path.starts_with(&root) || path == root {
        return Err(FleetdError::InvalidRequest(format!(
            "durable worktree {} is outside managed root {}",
            path.display(),
            root.display()
        )));
    }
    ensure_real_ancestor(path, &root)
}

async fn verify_materialized_worktree(
    base_repo: &Path,
    worktree: &Path,
    root: &Path,
) -> FleetdResult<()> {
    ensure_real_ancestor(worktree, &tokio::fs::canonicalize(root).await?)?;
    let base_common = git_common_dir(base_repo).await?;
    let worktree_common = git_common_dir(worktree).await?;
    if base_common != worktree_common {
        return Err(FleetdError::Conflict(format!(
            "managed path {} belongs to a different git repository",
            worktree.display()
        )));
    }
    Ok(())
}

async fn git_common_dir(path: &Path) -> FleetdResult<PathBuf> {
    let output = Command::new("git")
        .arg("-C")
        .arg(path)
        .args(["rev-parse", "--git-common-dir"])
        .output()
        .await?;
    if !output.status.success() {
        return Err(FleetdError::Conflict(format!(
            "{} is not a valid git worktree: {}",
            path.display(),
            bounded_stderr(&output.stderr)
        )));
    }
    let raw = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let resolved = if raw.is_absolute() {
        raw
    } else {
        path.join(raw)
    };
    Ok(tokio::fs::canonicalize(resolved).await?)
}

async fn remove_worktree(record: &WorktreeOwnershipRecord, root: &Path) -> FleetdResult<()> {
    ensure_record_is_scoped(record, root).await?;
    let path = Path::new(&record.path);
    match tokio::fs::symlink_metadata(path).await {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(FleetdError::Conflict(format!(
                "refusing cleanup of non-directory managed path {}",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    verify_materialized_worktree(Path::new(&record.base_repo), path, root).await?;
    let output = Command::new("git")
        .arg("-C")
        .arg(&record.base_repo)
        .args(["worktree", "remove", "--force"])
        .arg(path)
        .output()
        .await?;
    if !output.status.success() {
        return Err(FleetdError::Conflict(format!(
            "git worktree remove failed: {}",
            bounded_stderr(&output.stderr)
        )));
    }
    Ok(())
}

async fn seed_worktree_dirs(base: &Path, worktree: &Path, dirs: &[String]) -> Vec<String> {
    let mut outcomes = Vec::new();
    for raw in dirs {
        let rel = Path::new(raw);
        if rel.is_absolute()
            || rel.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            })
        {
            outcomes.push(format!("seed {raw}: refused (must be repo-relative)"));
            continue;
        }
        let source = base.join(rel);
        let destination = worktree.join(rel);
        if path_contains_symlink(base, &source) {
            outcomes.push(format!("seed {raw}: refused (source traverses a symlink)"));
            continue;
        }
        if !source.is_dir() {
            outcomes.push(format!("seed {raw}: skipped (missing in base repo)"));
            continue;
        }
        if destination.exists() {
            outcomes.push(format!("seed {raw}: skipped (already present)"));
            continue;
        }
        if let Some(parent) = destination.parent()
            && let Err(error) = tokio::fs::create_dir_all(parent).await
        {
            outcomes.push(format!(
                "seed {raw}: skipped (parent create failed: {error})"
            ));
            continue;
        }
        let mut command = Command::new("cp");
        #[cfg(target_os = "macos")]
        command.arg("-Rc");
        #[cfg(not(target_os = "macos"))]
        command.args(["-R", "--reflink=always"]);
        let output = command.arg(&source).arg(&destination).output().await;
        match output {
            Ok(output) if output.status.success() => {
                outcomes.push(format!("seed {raw}: cloned copy-on-write"));
            }
            Ok(output) => {
                let _ = tokio::fs::remove_dir_all(&destination).await;
                outcomes.push(format!(
                    "seed {raw}: skipped (cow clone failed: {})",
                    bounded_stderr(&output.stderr)
                ));
            }
            Err(error) => {
                let _ = tokio::fs::remove_dir_all(&destination).await;
                outcomes.push(format!("seed {raw}: skipped (cp spawn failed: {error})"));
            }
        }
    }
    outcomes
}

fn path_contains_symlink(root: &Path, path: &Path) -> bool {
    let Ok(relative) = path.strip_prefix(root) else {
        return true;
    };
    let mut cursor = root.to_path_buf();
    for component in relative.components() {
        cursor.push(component);
        match std::fs::symlink_metadata(&cursor) {
            Ok(metadata) if metadata.file_type().is_symlink() => return true,
            Ok(_) => {}
            Err(_) => return false,
        }
    }
    false
}

fn merge_seed_dirs(left: Vec<String>, right: Vec<String>) -> FleetdResult<Vec<String>> {
    let mut merged = BTreeSet::new();
    for value in left.into_iter().chain(right) {
        let path = Path::new(&value);
        if value.trim().is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|component| matches!(component, Component::ParentDir))
        {
            return Err(FleetdError::InvalidRequest(format!(
                "seed directory {value:?} must be a nonempty repo-relative path"
            )));
        }
        merged.insert(value);
    }
    Ok(merged.into_iter().collect())
}

fn inherited_process_basics() -> BTreeMap<String, String> {
    ["PATH", "HOME", "TMPDIR", "LANG", "LC_ALL"]
        .into_iter()
        .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value)))
        .collect()
}

pub(crate) fn looks_secret(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.contains("TOKEN")
        || key.contains("SECRET")
        || key.contains("PASSWORD")
        || key.ends_with("_KEY")
        || key == "ANTHROPIC_AUTH_TOKEN"
        || key == "OPENAI_API_KEY"
}

fn bounded_stderr(stderr: &[u8]) -> String {
    let rendered = String::from_utf8_lossy(stderr);
    rendered.chars().take(1_024).collect::<String>()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
// Fixture setup intentionally uses blocking fs/process calls outside service tasks.
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use bro_capabilities::{ExecutionKind, ExecutionServiceTier, ExecutionToolPolicy};
    use bro_core::{OperationId, Provider};
    use serde_json::json;

    #[test]
    fn requested_paths_and_seed_dirs_cannot_escape() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        assert!(resolve_managed_path(&root, Some("../escape"), &root, "task").is_err());
        assert!(resolve_managed_path(&root, Some("/tmp/escape"), &root, "task").is_err());
        assert!(merge_seed_dirs(vec!["../secret".into()], Vec::new()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_ancestor_is_never_authoritative() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let outside = root.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let managed = root.join("managed");
        std::fs::create_dir(&managed).unwrap();
        std::os::unix::fs::symlink(&outside, managed.join("link")).unwrap();
        assert!(
            resolve_managed_path(
                &managed,
                Some(managed.join("link/worktree").to_string_lossy().as_ref()),
                &root,
                "task"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn cow_seeding_never_plain_copies_or_traverses_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let base = root.join("base");
        let worktree = root.join("worktree");
        std::fs::create_dir_all(base.join("cache")).unwrap();
        std::fs::create_dir(&worktree).unwrap();
        std::fs::write(base.join("cache/artifact"), b"cached").unwrap();
        let outcomes =
            seed_worktree_dirs(&base, &worktree, &["cache".into(), "../escape".into()]).await;
        assert!(outcomes[1].contains("refused"));
        if worktree.join("cache/artifact").exists() {
            assert!(outcomes[0].contains("copy-on-write"));
        } else {
            assert!(outcomes[0].contains("skipped"));
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn restart_finishes_claim_before_create_and_fenced_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let repo = root.join("repo");
        std::fs::create_dir(&repo).unwrap();
        let run_git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {:?}: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        };
        run_git(&["init", "-q"]);
        std::fs::write(repo.join("README"), b"fixture\n").unwrap();
        run_git(&["add", "README"]);
        run_git(&[
            "-c",
            "user.name=Fleet Test",
            "-c",
            "user.email=fleet@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ]);

        let state = root.join("state");
        let worktree_root = root.join("worktrees");
        std::fs::create_dir(&worktree_root).unwrap();
        let worktree_root = worktree_root.canonicalize().unwrap();
        let path = worktree_root.join("claimed-before-create");
        let actor = AuthorityActor::open(state.join("fleet.json"), state.join("lock"), 1)
            .await
            .unwrap();
        let request = ExecutionRequest {
            operation_id: OperationId::new("operation-worktree-recovery"),
            idempotency_key: "worktree-recovery".into(),
            kind: ExecutionKind::Fresh {
                prompt: "fixture".into(),
            },
            provider: Provider::Glm,
            model: "glm-test".into(),
            effort: None,
            service_tier: ExecutionServiceTier::Default,
            code_mode: None,
            dispatch_context: None,
            working_set: WorkingSetIntent::CreateManagedWorktree {
                base_repo: repo.to_string_lossy().into_owned(),
                requested_path: Some(path.to_string_lossy().into_owned()),
                seed_dirs: Vec::new(),
            },
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy::default(),
            system_prompt: None,
            output_schema: None,
            labels: BTreeMap::new(),
        };
        let accepted = actor
            .call(move |authority| authority.admit_execution(request, 2).map_err(Into::into))
            .await
            .unwrap();
        let path_string = path.to_string_lossy().into_owned();
        let repo_string = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        let task_id = accepted.task_id.clone();
        let attempt_id = accepted.attempt_id.clone();
        actor
            .call(move |authority| {
                authority
                    .claim_worktree(
                        path_string,
                        repo_string,
                        &task_id,
                        Some(attempt_id),
                        Vec::new(),
                        3,
                    )
                    .map(|_| ())
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        assert!(!path.exists());
        actor.shutdown().await;

        let actor = AuthorityActor::open(state.join("fleet.json"), state.join("lock"), 4)
            .await
            .unwrap();
        let manager = WorktreeManager::new(worktree_root, None, actor.clone());
        manager.reconcile().await.unwrap();
        assert!(path.join("README").is_file());
        let snapshot = actor
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert!(snapshot.worktrees[path.to_string_lossy().as_ref()].materialized);

        let attempt_id = accepted.attempt_id.clone();
        actor
            .call(move |authority| {
                authority
                    .transition_attempt(
                        &attempt_id,
                        bro_capabilities::AttemptState::Running,
                        json!({}),
                        5,
                    )
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        let attempt_id = accepted.attempt_id.clone();
        actor
            .call(move |authority| {
                authority
                    .transition_attempt(
                        &attempt_id,
                        bro_capabilities::AttemptState::Completed,
                        json!({"ok": true}),
                        6,
                    )
                    .map_err(Into::into)
            })
            .await
            .unwrap();
        manager.reconcile().await.unwrap();
        assert!(!path.exists());
        let snapshot = actor
            .call(|authority| authority.snapshot().map_err(Into::into))
            .await
            .unwrap();
        assert_eq!(
            snapshot.worktrees[path.to_string_lossy().as_ref()].state,
            WorktreeOwnershipState::Released
        );
        actor.shutdown().await;
    }
}

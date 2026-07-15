use std::path::{Path, PathBuf};
use std::process::Stdio;

use async_trait::async_trait;
use bro_capabilities::{
    AgentForkTurns, ExecutionAccepted, ExecutionCodeMode, ExecutionKind, ExecutionRequest,
};
use bro_core::Provider;
use fleet_core::{ProviderSpawnEnvironment, ProvisionedWorker};
use tokio::process::Command;

use crate::sandbox::WorkerSandbox;
use crate::{FleetdError, FleetdResult};

const FLEET_OWNED_WORKER_ENV: &[&str] = &[
    "BRO_HOME",
    "BRO_HARNESS_PROVIDER",
    "BRO_HARNESS_SPAWN_SCRUB",
    "BRO_WORKER_SANDBOX",
];

pub struct LaunchSpec {
    pub accepted: ExecutionAccepted,
    pub request: ExecutionRequest,
    pub provisioned: ProvisionedWorker,
    /// Provider credentials/configuration exist only for process spawn. They
    /// are never serialized into the task, command, or shell environment.
    pub provider_environment: ProviderSpawnEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchedWorker {
    pub process_id: Option<u32>,
    pub event_log_path: PathBuf,
}

#[async_trait]
pub trait WorkerLauncher: Send + Sync {
    async fn launch(&self, spec: LaunchSpec) -> FleetdResult<LaunchedWorker>;
}

#[derive(Debug, Clone)]
pub struct ProcessWorkerLauncher {
    worker_socket: PathBuf,
    worker_root: PathBuf,
    bro_root: PathBuf,
    sandbox: WorkerSandbox,
}

pub struct ProcessWorkerLauncherConfig {
    pub binary: PathBuf,
    pub worker_socket: PathBuf,
    pub fleet_state_dir: PathBuf,
    pub worker_root: PathBuf,
    pub bro_root: PathBuf,
    pub protected_peer_service_roots: Vec<PathBuf>,
    pub service_token_file: PathBuf,
    pub denied_loopback_ports: Vec<u16>,
    pub external_sandbox_launcher: Option<PathBuf>,
}

impl ProcessWorkerLauncher {
    pub async fn new(config: ProcessWorkerLauncherConfig) -> FleetdResult<Self> {
        let ProcessWorkerLauncherConfig {
            binary,
            worker_socket,
            fleet_state_dir,
            worker_root,
            bro_root,
            protected_peer_service_roots,
            service_token_file,
            denied_loopback_ports,
            external_sandbox_launcher,
        } = config;
        let worker_root = create_private_directory(&worker_root).await?;
        let bro_root = create_private_directory(&bro_root).await?;
        let sandbox = WorkerSandbox::prepare(crate::sandbox::WorkerSandboxConfig {
            external_sandbox_launcher,
            worker_binary: binary,
            service_token_file,
            fleet_state_dir,
            worker_root: worker_root.clone(),
            bro_root: bro_root.clone(),
            protected_peer_service_roots,
            worker_socket: worker_socket.clone(),
            denied_loopback_ports,
        })
        .await?;
        Ok(Self {
            worker_socket,
            worker_root,
            bro_root,
            sandbox,
        })
    }

    pub fn event_log_path(&self, worker: &ProvisionedWorker) -> PathBuf {
        self.worker_root
            .join(worker.worker_id.as_str())
            .join("events.jsonl")
    }
}

#[async_trait]
impl WorkerLauncher for ProcessWorkerLauncher {
    async fn launch(&self, spec: LaunchSpec) -> FleetdResult<LaunchedWorker> {
        validate_provider_environment(&spec.provider_environment)?;
        let worker = &spec.provisioned;
        let worker_dir =
            create_private_directory(&self.worker_root.join(worker.worker_id.as_str())).await?;
        let bro_home =
            create_private_directory(&self.bro_root.join(spec.accepted.session_id.as_str()))
                .await?;
        let bootstrap_path = worker_dir.join("bootstrap.secret");
        let reconnect_path = worker_dir.join("reconnect.secret");
        let command_path = worker_dir.join("commands.jsonl");
        let event_path = worker_dir.join("events.jsonl");
        write_private_file(
            &bootstrap_path,
            worker.bootstrap_proof.expose_secret().as_bytes(),
        )
        .await?;

        let log_path = worker_dir.join("worker.log");
        let log_file = open_private_log(log_path).await?;
        let stderr = log_file
            .try_clone()
            .map_err(|error| FleetdError::WorkerLaunch(error.to_string()))?;

        let mut command = self.sandbox.command(
            &worker_dir,
            &bro_home,
            spec.provider_environment.protected_paths(),
        )?;
        command
            .env_clear()
            .envs(inherited_process_basics())
            .kill_on_drop(false)
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr));

        for (key, value) in spec.provider_environment.expose() {
            command.env(key, value);
        }

        // Apply fleet-owned identity and scrub state last. Collision rejection
        // above is the fail-closed contract; last-write ordering is defense in
        // depth if a future ProviderSpawnEnvironment constructor misses it.
        command
            .env("BRO_WORKER_SANDBOX", self.sandbox.kind_name())
            .env("BRO_HOME", &bro_home)
            .env("BRO_HARNESS_PROVIDER", spec.request.provider.as_str())
            .env(
                "BRO_HARNESS_SPAWN_SCRUB",
                spawn_scrub_keys(spec.provider_environment.expose().keys()),
            )
            .arg("--worker")
            .arg("--daemon-worker")
            .arg("--fleet-socket")
            .arg(&self.worker_socket)
            .arg("--task-id")
            .arg(spec.accepted.task_id.as_str())
            .arg("--worker-id")
            .arg(worker.worker_id.as_str())
            .arg("--bootstrap-secret-file")
            .arg(&bootstrap_path)
            .arg("--worker-reconnect-credential")
            .arg(&reconnect_path)
            .arg("--worker-command-journal")
            .arg(&command_path)
            .arg("--worker-event-log")
            .arg(&event_path)
            .arg("--output-format")
            .arg("stream-json")
            .arg("--input-format")
            .arg("stream-json")
            .arg("--session-id")
            .arg(spec.accepted.session_id.as_str())
            .arg("--model")
            .arg(&spec.request.model);

        apply_request_args(&mut command, &spec.request)?;
        apply_provider_transport(&mut command, spec.request.provider)?;

        let child = command
            .spawn()
            .map_err(|error| FleetdError::WorkerLaunch(error.to_string()))?;
        Ok(LaunchedWorker {
            process_id: child.id(),
            event_log_path: event_path,
        })
    }
}

fn validate_provider_environment(environment: &ProviderSpawnEnvironment) -> FleetdResult<()> {
    let collisions = FLEET_OWNED_WORKER_ENV
        .iter()
        .filter(|key| environment.expose().contains_key(**key))
        .copied()
        .collect::<Vec<_>>();
    if collisions.is_empty() {
        return Ok(());
    }
    Err(FleetdError::InvalidConfiguration(format!(
        "provider environment cannot override fleet-owned worker variables: {}",
        collisions.join(", ")
    )))
}

fn apply_request_args(command: &mut Command, request: &ExecutionRequest) -> FleetdResult<()> {
    match &request.kind {
        ExecutionKind::Fresh { .. } => {}
        ExecutionKind::Resume { session_id, .. } | ExecutionKind::MailboxResume { session_id } => {
            command.arg("--resume").arg(session_id.as_str());
        }
    }
    match &request.working_set {
        bro_capabilities::WorkingSetIntent::Scratch => {}
        bro_capabilities::WorkingSetIntent::Existing { cwd, .. } => {
            command.arg("--cwd").arg(cwd);
        }
        bro_capabilities::WorkingSetIntent::CreateManagedWorktree { requested_path, .. } => {
            let cwd = requested_path.as_ref().ok_or_else(|| {
                FleetdError::InvalidRequest(
                    "managed worktree request must be resolved before worker launch".into(),
                )
            })?;
            command.arg("--cwd").arg(cwd);
        }
    }
    if let Some(effort) = &request.effort {
        command.arg("--effort").arg(effort);
    }
    command
        .arg("--service-tier")
        .arg(match request.service_tier {
            bro_capabilities::ExecutionServiceTier::Default => "default",
            bro_capabilities::ExecutionServiceTier::Priority => "priority",
            bro_capabilities::ExecutionServiceTier::Flex => "flex",
        });
    if let Some(code_mode) = request.code_mode {
        command.arg("--code-mode").arg(match code_mode {
            ExecutionCodeMode::Off => "off",
            ExecutionCodeMode::Optional => "optional",
            ExecutionCodeMode::Only => "only",
        });
    }
    if let Some(system_prompt) = &request.system_prompt {
        command.arg("--system-prompt").arg(system_prompt);
    }
    if let Some(schema) = &request.output_schema {
        command
            .arg("--output-schema")
            .arg(serde_json::to_string(schema)?);
    }
    if let Some(context) = &request.dispatch_context {
        command
            .arg("--dispatch-context")
            .arg(serde_json::to_string(context)?);
    }
    if !request.shell_env.is_empty() {
        command
            .arg("--shell-env")
            .arg(serde_json::to_string(&request.shell_env)?);
    }
    if !request.tool_policy.allow_tools.is_empty() {
        command
            .arg("--allow-tools")
            .arg(request.tool_policy.allow_tools.join(","));
    }
    if !request.tool_policy.deny_tools.is_empty() {
        command
            .arg("--deny-tools")
            .arg(request.tool_policy.deny_tools.join(","));
    }
    if !request.tool_policy.tool_defaults.is_empty() {
        command
            .arg("--additional-context")
            .arg(serde_json::to_string(&request.tool_policy.tool_defaults)?);
    }
    apply_internal_agent_launch_args(command, &request.kind, &request.labels)?;
    Ok(())
}

fn apply_internal_agent_launch_args(
    command: &mut Command,
    kind: &ExecutionKind,
    labels: &std::collections::BTreeMap<String, String>,
) -> FleetdResult<()> {
    let source = labels.get("fork_source_session");
    let turns = labels.get("fork_turns");
    match (source, turns) {
        (None, None) => {}
        (Some(source), Some(turns)) => {
            if !matches!(kind, ExecutionKind::Fresh { .. }) {
                return Err(FleetdError::InvalidRequest(
                    "history fork metadata is valid only for a fresh execution".into(),
                ));
            }
            if source.is_empty() {
                return Err(FleetdError::InvalidRequest(
                    "history fork source session is empty".into(),
                ));
            }
            let policy: AgentForkTurns = serde_json::from_str(turns).map_err(|_| {
                FleetdError::InvalidRequest("history fork turn policy is invalid".into())
            })?;
            if matches!(policy, AgentForkTurns::Recent(0)) {
                return Err(FleetdError::InvalidRequest(
                    "history fork recent turn count must be positive".into(),
                ));
            }
            if labels
                .get("prompt_cache_root")
                .is_none_or(|root| root.is_empty())
            {
                return Err(FleetdError::InvalidRequest(
                    "history fork is missing prompt-cache root identity".into(),
                ));
            }
            command
                .arg("--fork-source-session")
                .arg(source)
                .arg("--fork-turns")
                .arg(turns);
        }
        _ => {
            return Err(FleetdError::InvalidRequest(
                "history fork metadata is incomplete".into(),
            ));
        }
    }
    if let Some(root) = labels.get("prompt_cache_root") {
        if root.is_empty() {
            return Err(FleetdError::InvalidRequest(
                "prompt-cache root identity is empty".into(),
            ));
        }
        command.arg("--prompt-cache-root").arg(root);
    }
    Ok(())
}

fn apply_provider_transport(command: &mut Command, provider: Provider) -> FleetdResult<()> {
    match provider {
        Provider::Glm | Provider::Deepseek | Provider::Minimax => {
            command.env("BRO_HARNESS_TRANSPORT", "anthropic");
        }
        Provider::Brodex => {
            command.env("BRO_HARNESS_TRANSPORT", "openai-responses");
        }
        Provider::VibeBh => {
            command
                .env("BRO_HARNESS_TRANSPORT", "openai-chat")
                .env("BRO_HARNESS_CHAT_REASONING", "mistral");
        }
        Provider::Workflow => {
            return Err(FleetdError::InvalidRequest(
                "workflow is not a worker provider".into(),
            ));
        }
    }
    Ok(())
}

fn spawn_scrub_keys<'a>(provider_keys: impl Iterator<Item = &'a String>) -> String {
    let mut keys = std::collections::BTreeSet::from([
        "ANTHROPIC_API_KEY".to_string(),
        "ANTHROPIC_AUTH_TOKEN".to_string(),
        "ANTHROPIC_BASE_URL".to_string(),
        "OPENAI_API_KEY".to_string(),
        "BRO_HOME".to_string(),
        "BRO_HARNESS_PROVIDER".to_string(),
        "BRO_HARNESS_TRANSPORT".to_string(),
        "BRO_HARNESS_SPAWN_SCRUB".to_string(),
    ]);
    for (key, _) in std::env::vars_os() {
        let key = key.to_string_lossy();
        let uppercase = key.to_ascii_uppercase();
        if uppercase.contains("KEY")
            || uppercase.contains("TOKEN")
            || uppercase.contains("SECRET")
            || uppercase.contains("PASSWORD")
            || uppercase.starts_with("BLACKBOX_")
            || uppercase.starts_with("BBOX_")
            || uppercase.starts_with("FLEETD_")
        {
            keys.insert(key.into_owned());
        }
    }
    keys.extend(provider_keys.cloned());
    keys.into_iter().collect::<Vec<_>>().join(",")
}

fn inherited_process_basics() -> std::collections::BTreeMap<String, String> {
    let mut basics: std::collections::BTreeMap<String, String> =
        ["HOME", "TMPDIR", "LANG", "LC_ALL", "SSL_CERT_FILE"]
            .into_iter()
            .filter_map(|key| std::env::var(key).ok().map(|value| (key.into(), value)))
            .collect();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let inherited = std::env::var_os("PATH");
    basics.insert(
        "PATH".into(),
        augmented_worker_path(home.as_deref(), inherited.as_deref())
            .to_string_lossy()
            .into_owned(),
    );
    basics
}

fn augmented_worker_path(
    home: Option<&Path>,
    inherited: Option<&std::ffi::OsStr>,
) -> std::ffi::OsString {
    const SYSTEM_PATH: &str = "/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin";
    let mut candidates = Vec::new();
    if let Some(home) = home {
        candidates.push(home.join(".local/bin"));
        candidates.push(home.join(".cargo/bin"));
    }
    candidates.push(PathBuf::from("/opt/homebrew/bin"));
    candidates.push(PathBuf::from("/opt/homebrew/sbin"));
    candidates.extend(std::env::split_paths(
        inherited.unwrap_or_else(|| std::ffi::OsStr::new(SYSTEM_PATH)),
    ));

    let mut seen = std::collections::BTreeSet::new();
    let unique = candidates
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .filter(|path| seen.insert(path.clone()));
    std::env::join_paths(unique).unwrap_or_else(|_| std::ffi::OsString::from(SYSTEM_PATH))
}

async fn create_private_directory(path: &Path) -> FleetdResult<PathBuf> {
    tokio::fs::create_dir_all(path).await?;
    let metadata = tokio::fs::symlink_metadata(path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "private fleet path {} is not a real directory",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    path.canonicalize().map_err(FleetdError::from)
}

async fn write_private_file(path: &Path, bytes: &[u8]) -> FleetdResult<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        options.mode(0o600);
    }
    let mut file = options.open(path).await?;
    use tokio::io::AsyncWriteExt;
    file.write_all(bytes).await?;
    file.write_all(b"\n").await?;
    file.sync_all().await?;
    Ok(())
}

async fn open_private_log(path: PathBuf) -> FleetdResult<std::fs::File> {
    tokio::task::spawn_blocking(move || {
        let mut options = std::fs::OpenOptions::new();
        options.append(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(path).map_err(FleetdError::from)
    })
    .await
    .map_err(|error| FleetdError::WorkerLaunch(error.to_string()))?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(command: &Command) -> Vec<String> {
        command
            .as_std()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn agent_history_and_cache_labels_reach_harness_internal_args() {
        let mut command = Command::new("bro-harness");
        let labels = std::collections::BTreeMap::from([
            ("fork_source_session".into(), "parent-session".into()),
            (
                "fork_turns".into(),
                serde_json::to_string(&AgentForkTurns::Recent(2)).unwrap(),
            ),
            ("prompt_cache_root".into(), "agent-root:root-1".into()),
            ("logical_parent".into(), "/root".into()),
        ]);

        apply_internal_agent_launch_args(
            &mut command,
            &ExecutionKind::Fresh {
                prompt: "work".into(),
            },
            &labels,
        )
        .unwrap();

        let args = args(&command);
        assert_eq!(
            args,
            vec![
                "--fork-source-session",
                "parent-session",
                "--fork-turns",
                r#"{"kind":"recent","turns":2}"#,
                "--prompt-cache-root",
                "agent-root:root-1",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "logical_parent"));
        assert!(!args.iter().any(|arg| arg == "/root"));
    }

    #[test]
    fn incomplete_or_invalid_fork_labels_fail_closed() {
        let fresh = ExecutionKind::Fresh {
            prompt: "work".into(),
        };
        for labels in [
            std::collections::BTreeMap::from([("fork_source_session".into(), "parent".into())]),
            std::collections::BTreeMap::from([
                ("fork_source_session".into(), "parent".into()),
                ("fork_turns".into(), "not-json".into()),
                ("prompt_cache_root".into(), "root".into()),
            ]),
            std::collections::BTreeMap::from([
                ("fork_source_session".into(), "parent".into()),
                (
                    "fork_turns".into(),
                    serde_json::to_string(&AgentForkTurns::All).unwrap(),
                ),
            ]),
        ] {
            assert!(
                apply_internal_agent_launch_args(
                    &mut Command::new("bro-harness"),
                    &fresh,
                    &labels,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn provider_environment_cannot_override_fleet_owned_session_or_scrub_state() {
        let environment = ProviderSpawnEnvironment::new(std::collections::BTreeMap::from([
            ("BRO_HOME".into(), "/tmp/provider-controlled".into()),
            (
                "BRO_HARNESS_SPAWN_SCRUB".into(),
                "ONLY_PROVIDER_CHOSEN_KEYS".into(),
            ),
        ]));

        assert!(matches!(
            validate_provider_environment(&environment),
            Err(FleetdError::InvalidConfiguration(message))
                if message.contains("BRO_HOME")
                    && message.contains("BRO_HARNESS_SPAWN_SCRUB")
        ));
    }

    #[test]
    fn absent_inherited_path_keeps_user_tools_and_system_defaults() {
        let path = augmented_worker_path(Some(Path::new("/Users/test")), None);
        assert_eq!(
            path.to_string_lossy(),
            "/Users/test/.local/bin:/Users/test/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin"
        );
    }

    #[test]
    fn augmented_worker_path_stably_removes_duplicates() {
        let inherited =
            std::ffi::OsStr::new("/usr/bin:/Users/test/.local/bin:/opt/homebrew/bin:/bin:/usr/bin");
        let path = augmented_worker_path(Some(Path::new("/Users/test")), Some(inherited));
        assert_eq!(
            path.to_string_lossy(),
            "/Users/test/.local/bin:/Users/test/.cargo/bin:/opt/homebrew/bin:/opt/homebrew/sbin:/usr/bin:/bin"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[allow(clippy::disallowed_methods)]
    async fn private_fleet_directories_are_canonical_0700_and_not_symlinks() {
        use std::os::unix::fs::{PermissionsExt as _, symlink};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let requested = root.join("state/session");
        let canonical = create_private_directory(&requested).await.unwrap();
        assert_eq!(canonical, requested.canonicalize().unwrap());
        assert_eq!(
            canonical.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );

        let alias = root.join("session-alias");
        symlink(&canonical, &alias).unwrap();
        assert!(matches!(
            create_private_directory(&alias).await,
            Err(FleetdError::InvalidConfiguration(message))
                if message.contains("not a real directory")
        ));
    }
}

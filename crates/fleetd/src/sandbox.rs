use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;

use crate::{FleetdError, FleetdResult};

#[cfg(target_os = "macos")]
const MACOS_SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";
#[cfg(target_os = "linux")]
const EXTERNAL_LAUNCHER_PROTOCOL: &str = "blackbox-worker-sandbox-v1";

#[derive(Debug, Clone)]
pub(crate) struct WorkerSandbox {
    backend: WorkerSandboxBackend,
    worker_binary: PathBuf,
    service_token_file: PathBuf,
    fleet_state_dir: PathBuf,
    worker_root: PathBuf,
    bro_root: PathBuf,
    protected_peer_service_roots: Vec<PathBuf>,
    worker_socket: PathBuf,
    denied_service_ports: Vec<u16>,
}

pub(crate) struct WorkerSandboxConfig {
    pub(crate) external_sandbox_launcher: Option<PathBuf>,
    pub(crate) worker_binary: PathBuf,
    pub(crate) service_token_file: PathBuf,
    pub(crate) fleet_state_dir: PathBuf,
    pub(crate) worker_root: PathBuf,
    pub(crate) bro_root: PathBuf,
    pub(crate) protected_peer_service_roots: Vec<PathBuf>,
    pub(crate) worker_socket: PathBuf,
    pub(crate) denied_service_ports: Vec<u16>,
}

struct WorkerSandboxScope {
    worker_dir: PathBuf,
    bro_session_dir: PathBuf,
    protected_paths: Vec<PathBuf>,
    #[cfg(target_os = "macos")]
    protected_entry_patterns: Vec<String>,
}

#[derive(Debug, Clone)]
enum WorkerSandboxBackend {
    #[cfg(target_os = "macos")]
    MacOsSeatbelt,
    #[cfg(target_os = "linux")]
    External(PathBuf),
}

impl WorkerSandbox {
    pub(crate) async fn prepare(config: WorkerSandboxConfig) -> FleetdResult<Self> {
        let WorkerSandboxConfig {
            external_sandbox_launcher: external_launcher,
            worker_binary,
            service_token_file,
            fleet_state_dir,
            worker_root,
            bro_root,
            protected_peer_service_roots,
            worker_socket,
            denied_service_ports,
        } = config;
        let worker_binary = canonical_protected_file(&worker_binary, "bro-harness worker binary")?;
        let service_token_file =
            canonical_protected_file(&service_token_file, "shared service token")?;
        let fleet_state_dir =
            ensure_private_directory(fleet_state_dir, "fleet state directory").await?;
        let worker_root = ensure_private_directory(worker_root, "fleet worker root").await?;
        if !worker_root.starts_with(&fleet_state_dir) {
            return Err(FleetdError::InvalidConfiguration(
                "fleet worker root must be contained by the fleet state directory".into(),
            ));
        }
        let bro_root = ensure_private_directory(bro_root, "fleet Bro session root").await?;
        if !bro_root.starts_with(&fleet_state_dir) {
            return Err(FleetdError::InvalidConfiguration(
                "fleet Bro session root must be contained by the fleet state directory".into(),
            ));
        }
        let mut protected_peer_service_roots = protected_peer_service_roots
            .into_iter()
            .map(|path| canonical_protected_root(&path, "peer service authority root"))
            .collect::<FleetdResult<Vec<_>>>()?;
        protected_peer_service_roots.sort();
        protected_peer_service_roots.dedup();
        if let Some(path) = protected_peer_service_roots
            .iter()
            .find(|path| path.starts_with(&fleet_state_dir))
        {
            return Err(FleetdError::InvalidConfiguration(format!(
                "peer service authority root {} must not be inside the fleet state directory",
                path.display()
            )));
        }
        let socket_parent = worker_socket.parent().ok_or_else(|| {
            FleetdError::InvalidConfiguration(
                "fleet worker socket has no containing directory".into(),
            )
        })?;
        ensure_private_directory(socket_parent.to_path_buf(), "fleet worker socket directory")
            .await?;
        let worker_socket = canonical_future_entry(&worker_socket, "fleet worker socket")?;
        if !worker_socket.starts_with(&fleet_state_dir) {
            return Err(FleetdError::InvalidConfiguration(
                "fleet worker socket must be contained by the fleet state directory".into(),
            ));
        }
        let denied_service_ports = denied_service_ports
            .into_iter()
            .filter(|port| *port != 0)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();

        #[cfg(target_os = "macos")]
        {
            if external_launcher.is_some() {
                return Err(FleetdError::InvalidConfiguration(
                    "macOS workers must use the built-in Seatbelt launcher".into(),
                ));
            }
            validate_root_owned_executable(Path::new(MACOS_SANDBOX_EXEC), "sandbox-exec")?;
            let sandbox = Self {
                backend: WorkerSandboxBackend::MacOsSeatbelt,
                worker_binary,
                service_token_file,
                fleet_state_dir,
                worker_root,
                bro_root,
                protected_peer_service_roots,
                worker_socket,
                denied_service_ports,
            };
            sandbox.probe_macos_seatbelt().await?;
            Ok(sandbox)
        }

        #[cfg(target_os = "linux")]
        {
            let launcher = external_launcher.ok_or_else(|| {
                FleetdError::InvalidConfiguration(
                    "Linux authority workers require FLEETD_WORKER_SANDBOX_LAUNCHER".into(),
                )
            })?;
            let launcher =
                validate_root_owned_executable(&launcher, "external worker sandbox launcher")?;
            let sandbox = Self {
                backend: WorkerSandboxBackend::External(launcher),
                worker_binary,
                service_token_file,
                fleet_state_dir,
                worker_root,
                bro_root,
                protected_peer_service_roots,
                worker_socket,
                denied_service_ports,
            };
            sandbox.probe_external_launcher().await?;
            Ok(sandbox)
        }

        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = external_launcher;
            let _ = worker_socket;
            let _ = denied_service_ports;
            Err(FleetdError::InvalidConfiguration(
                "authority workers have no supported process sandbox on this platform".into(),
            ))
        }
    }

    pub(crate) fn command<I, P>(
        &self,
        worker_dir: &Path,
        bro_session_dir: &Path,
        protected_paths: I,
    ) -> FleetdResult<Command>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let scope = self.scope(worker_dir, bro_session_dir, protected_paths)?;
        match &self.backend {
            #[cfg(target_os = "macos")]
            WorkerSandboxBackend::MacOsSeatbelt => {
                let mut command = Command::new(MACOS_SANDBOX_EXEC);
                self.apply_macos_seatbelt_args(&mut command, &scope);
                command.arg("--").arg(&self.worker_binary);
                Ok(command)
            }
            #[cfg(target_os = "linux")]
            WorkerSandboxBackend::External(launcher) => {
                let mut command = Command::new(launcher);
                self.apply_external_launcher_args(&mut command, &scope);
                command.arg("--").arg(&self.worker_binary);
                Ok(command)
            }
        }
    }

    fn scope<I, P>(
        &self,
        worker_dir: &Path,
        bro_session_dir: &Path,
        protected_paths: I,
    ) -> FleetdResult<WorkerSandboxScope>
    where
        I: IntoIterator<Item = P>,
        P: AsRef<Path>,
    {
        let worker_dir = canonical_private_directory(worker_dir, "worker directory")?;
        if worker_dir.parent() != Some(self.worker_root.as_path()) {
            return Err(FleetdError::InvalidConfiguration(
                "worker directory must be an immediate child of the worker root".into(),
            ));
        }
        let bro_session_dir =
            canonical_private_directory(bro_session_dir, "Bro session directory")?;
        if bro_session_dir.parent() != Some(self.bro_root.as_path()) {
            return Err(FleetdError::InvalidConfiguration(
                "Bro session directory must be an immediate child of the Bro session root".into(),
            ));
        }
        let mut protected_paths = protected_paths
            .into_iter()
            .map(|path| canonical_protected_path(path.as_ref(), "provider credential source"))
            .collect::<FleetdResult<Vec<_>>>()?;
        protected_paths.sort();
        protected_paths.dedup();
        #[cfg(target_os = "macos")]
        let protected_entry_patterns = protected_entry_patterns(
            protected_paths
                .iter()
                .map(PathBuf::as_path)
                .chain([
                    self.service_token_file.as_path(),
                    self.worker_binary.as_path(),
                    self.worker_socket.as_path(),
                ])
                .chain(
                    self.protected_peer_service_roots
                        .iter()
                        .map(PathBuf::as_path),
                ),
        )?;
        Ok(WorkerSandboxScope {
            worker_dir,
            bro_session_dir,
            protected_paths,
            #[cfg(target_os = "macos")]
            protected_entry_patterns,
        })
    }

    pub(crate) fn kind_name(&self) -> &'static str {
        match &self.backend {
            #[cfg(target_os = "macos")]
            WorkerSandboxBackend::MacOsSeatbelt => "macos-seatbelt",
            #[cfg(target_os = "linux")]
            WorkerSandboxBackend::External(_) => "external-v1",
        }
    }

    #[cfg(target_os = "macos")]
    fn apply_macos_seatbelt_args(&self, command: &mut Command, scope: &WorkerSandboxScope) {
        command
            .arg("-p")
            .arg(macos_seatbelt_profile(
                &self.denied_service_ports,
                scope.protected_paths.len(),
                self.protected_peer_service_roots.len(),
                scope.protected_entry_patterns.len(),
            ))
            .arg(format!(
                "-DSERVICE_TOKEN_FILE={}",
                self.service_token_file
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DWORKER_BINARY={}",
                self.worker_binary
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DFLEET_STATE_DIR={}",
                self.fleet_state_dir
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DWORKER_ROOT={}",
                self.worker_root
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DWORKER_DIR={}",
                scope
                    .worker_dir
                    .to_str()
                    .expect("sandbox paths were validated during launch")
            ))
            .arg(format!(
                "-DBRO_ROOT={}",
                self.bro_root
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DBRO_SESSION_DIR={}",
                scope
                    .bro_session_dir
                    .to_str()
                    .expect("sandbox paths were validated during launch")
            ))
            .arg(format!(
                "-DWORKER_SOCKET={}",
                self.worker_socket
                    .to_str()
                    .expect("sandbox paths were validated during prepare")
            ))
            .arg(format!(
                "-DAUTHORITY_ENTRY_PATTERN={}",
                authority_entry_pattern(&self.fleet_state_dir)
                    .expect("sandbox paths were validated during prepare")
            ));
        for (index, path) in scope.protected_paths.iter().enumerate() {
            command.arg(format!(
                "-DPROTECTED_PATH_{index}={}",
                path.to_str()
                    .expect("sandbox paths were validated during launch")
            ));
        }
        for (index, path) in self.protected_peer_service_roots.iter().enumerate() {
            command.arg(format!(
                "-DPROTECTED_SERVICE_ROOT_{index}={}",
                path.to_str()
                    .expect("sandbox paths were validated during prepare")
            ));
        }
        for (index, pattern) in scope.protected_entry_patterns.iter().enumerate() {
            command.arg(format!("-DPROTECTED_ENTRY_PATTERN_{index}={pattern}"));
        }
    }

    #[cfg(target_os = "macos")]
    async fn probe_macos_seatbelt(&self) -> FleetdResult<()> {
        let probe_dir = ensure_private_directory(
            self.worker_root.join(".sandbox-probe"),
            "sandbox probe worker directory",
        )
        .await?;
        let probe_bro_dir = ensure_private_directory(
            self.bro_root.join(".sandbox-probe"),
            "sandbox probe Bro session directory",
        )
        .await?;
        let scope = self.scope(&probe_dir, &probe_bro_dir, std::iter::empty::<&Path>())?;
        let mut command = Command::new(MACOS_SANDBOX_EXEC);
        self.apply_macos_seatbelt_args(&mut command, &scope);
        command
            .arg("--")
            .arg("/usr/bin/true")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = command.status().await.map_err(|error| {
            FleetdError::InvalidConfiguration(format!(
                "starting the required macOS worker sandbox probe failed: {error}"
            ))
        })?;
        let _ = tokio::fs::remove_dir(&probe_dir).await;
        let _ = tokio::fs::remove_dir(&probe_bro_dir).await;
        if !status.success() {
            return Err(FleetdError::InvalidConfiguration(format!(
                "the required macOS worker sandbox rejected its policy (status {status})"
            )));
        }
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn apply_external_launcher_args(&self, command: &mut Command, scope: &WorkerSandboxScope) {
        command
            .arg("--launch")
            .arg("--protocol")
            .arg(EXTERNAL_LAUNCHER_PROTOCOL)
            .arg("--service-token-file")
            .arg(&self.service_token_file)
            .arg("--worker-binary")
            .arg(&self.worker_binary)
            .arg("--worker-socket")
            .arg(&self.worker_socket)
            .arg("--fleet-state-dir")
            .arg(&self.fleet_state_dir)
            .arg("--worker-root")
            .arg(&self.worker_root)
            .arg("--worker-dir")
            .arg(&scope.worker_dir)
            .arg("--bro-root")
            .arg(&self.bro_root)
            .arg("--bro-session-dir")
            .arg(&scope.bro_session_dir);
        for path in &self.protected_peer_service_roots {
            command.arg("--protected-service-root").arg(path);
        }
        for path in &scope.protected_paths {
            command.arg("--protected-path").arg(path);
        }
        for port in &self.denied_service_ports {
            command.arg("--deny-loopback-port").arg(port.to_string());
        }
    }

    #[cfg(target_os = "linux")]
    async fn probe_external_launcher(&self) -> FleetdResult<()> {
        let WorkerSandboxBackend::External(launcher) = &self.backend;
        let probe_dir = ensure_private_directory(
            self.worker_root.join(".sandbox-probe"),
            "sandbox probe worker directory",
        )
        .await?;
        let probe_bro_dir = ensure_private_directory(
            self.bro_root.join(".sandbox-probe"),
            "sandbox probe Bro session directory",
        )
        .await?;
        let scope = self.scope(&probe_dir, &probe_bro_dir, std::iter::empty::<&Path>())?;
        let mut command = Command::new(launcher);
        command
            .arg("--self-test")
            .arg("--protocol")
            .arg(EXTERNAL_LAUNCHER_PROTOCOL)
            .arg("--service-token-file")
            .arg(&self.service_token_file)
            .arg("--worker-binary")
            .arg(&self.worker_binary)
            .arg("--worker-socket")
            .arg(&self.worker_socket)
            .arg("--fleet-state-dir")
            .arg(&self.fleet_state_dir)
            .arg("--worker-root")
            .arg(&self.worker_root)
            .arg("--worker-dir")
            .arg(&scope.worker_dir)
            .arg("--bro-root")
            .arg(&self.bro_root)
            .arg("--bro-session-dir")
            .arg(&scope.bro_session_dir);
        for path in &self.protected_peer_service_roots {
            command.arg("--protected-service-root").arg(path);
        }
        for path in &scope.protected_paths {
            command.arg("--protected-path").arg(path);
        }
        for port in &self.denied_service_ports {
            command.arg("--deny-loopback-port").arg(port.to_string());
        }
        let output = command
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .await
            .map_err(|error| {
                FleetdError::InvalidConfiguration(format!(
                    "starting the required Linux worker sandbox self-test failed: {error}"
                ))
            })?;
        let _ = tokio::fs::remove_dir(&probe_dir).await;
        let _ = tokio::fs::remove_dir(&probe_bro_dir).await;
        if !output.status.success()
            || output.stdout != format!("{EXTERNAL_LAUNCHER_PROTOCOL}\n").as_bytes()
        {
            return Err(FleetdError::InvalidConfiguration(
                "the required Linux worker sandbox launcher failed its v1 self-test".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn macos_seatbelt_profile(
    denied_service_ports: &[u16],
    protected_path_count: usize,
    protected_service_root_count: usize,
    protected_entry_pattern_count: usize,
) -> String {
    let mut profile = String::from(
        r#"(version 1)
(allow default)
; The bearer is daemon-only authority. This path rule is inherited by every
; shell, compiler, and helper process the harness launches.
(deny file-read* (literal (param "SERVICE_TOKEN_FILE")))
(deny file-write* (literal (param "SERVICE_TOKEN_FILE")))
; A worker cannot replace the executable trusted by the next fleet dispatch.
(deny file-write* (literal (param "WORKER_BINARY")))
; Fleet authority is concentrated in state-root files. Workers may traverse
; the root to reach managed worktrees and their own journals, but may neither
; read nor mutate current/future authority snapshot and lock entries.
(deny file-read* (regex (param "AUTHORITY_ENTRY_PATTERN")))
(deny file-write* (regex (param "AUTHORITY_ENTRY_PATTERN")))
; Peer worker directories contain bootstrap/reconnect proofs and journals.
; Keep the current worker subtree available while denying every sibling.
(deny file-read*
  (require-all
    (subpath (param "WORKER_ROOT"))
    (require-not (literal (param "WORKER_DIR")))
    (require-not (subpath (param "WORKER_DIR")))))
(deny file-write*
  (require-all
    (subpath (param "WORKER_ROOT"))
    (require-not (literal (param "WORKER_DIR")))
    (require-not (subpath (param "WORKER_DIR")))))
; Provider session state is durable across a replacement worker for the same
; SessionId, but is not shared between sessions. This prevents a worker from
; reading or corrupting a peer session's provider transcripts and resumable
; authentication state.
(deny file-read*
  (require-all
    (subpath (param "BRO_ROOT"))
    (require-not (literal (param "BRO_SESSION_DIR")))
    (require-not (subpath (param "BRO_SESSION_DIR")))))
(deny file-write*
  (require-all
    (subpath (param "BRO_ROOT"))
    (require-not (literal (param "BRO_SESSION_DIR")))
    (require-not (subpath (param "BRO_SESSION_DIR")))))
; Unix-domain connect is a network operation. Denying file mutation on the
; socket path prevents unlink/replacement without blocking the connection.
(deny file-write* (literal (param "WORKER_SOCKET")))
; A literal rule protects the current vnode name, but a same-UID process could
; otherwise rename a writable ancestor and recover the same token or binary at
; an unprotected alias. Each regex covers exactly one directory-entry level on
; the canonical path chain; descendant repository and journal writes remain
; available.
; Do not let a same-UID worker recover daemon authority from another process
; or signal processes outside its inherited sandbox domain.
(deny mach-priv-task-port)
(deny process-info* (target others))
(deny signal (target others))
"#,
    );
    for index in 0..protected_path_count {
        profile.push_str(&format!(
            "(deny file-read* (literal (param \"PROTECTED_PATH_{index}\")))\n\
             (deny file-read* (subpath (param \"PROTECTED_PATH_{index}\")))\n\
             (deny file-write* (literal (param \"PROTECTED_PATH_{index}\")))\n\
             (deny file-write* (subpath (param \"PROTECTED_PATH_{index}\")))\n"
        ));
    }
    for index in 0..protected_service_root_count {
        profile.push_str(&format!(
            "; Peer service root {index}. A shared corpus-state parent may contain\n\
             ; FLEET_STATE_DIR, whose narrower worker/session policy remains active.\n\
             (deny file-read* (literal (param \"PROTECTED_SERVICE_ROOT_{index}\")))\n\
             (deny file-read*\n\
               (require-all\n\
                 (subpath (param \"PROTECTED_SERVICE_ROOT_{index}\"))\n\
                 (require-not (literal (param \"FLEET_STATE_DIR\")))\n\
                 (require-not (subpath (param \"FLEET_STATE_DIR\")))))\n\
             (deny file-write* (literal (param \"PROTECTED_SERVICE_ROOT_{index}\")))\n\
             (deny file-write*\n\
               (require-all\n\
                 (subpath (param \"PROTECTED_SERVICE_ROOT_{index}\"))\n\
                 (require-not (literal (param \"FLEET_STATE_DIR\")))\n\
                 (require-not (subpath (param \"FLEET_STATE_DIR\")))))\n"
        ));
    }
    for index in 0..protected_entry_pattern_count {
        profile.push_str(&format!(
            "(deny file-write* (regex (param \"PROTECTED_ENTRY_PATTERN_{index}\")))\n"
        ));
    }
    // Deny each service port on EVERY host, not just loopback. Seatbelt's
    // network filter only accepts `*` or `localhost` as the host component
    // (a specific remote IP is rejected at profile-compile time), so a wildcard
    // host is the only way to express "the worker may not reach the corpus
    // endpoint" when that endpoint is a remote cluster service rather than a
    // loopback tunnel port. These ports are dedicated blackbox service ports
    // (7264-7266 plus any configured upstream port), never a port a worker
    // legitimately needs, so wildcarding the host is precise enough and leaves
    // provider egress on 443 untouched. Loopback tunnel ports remain covered
    // because `*:port` is a strict superset of `localhost:port`.
    for port in denied_service_ports {
        profile.push_str(&format!(
            "(deny network-outbound (remote ip \"*:{port}\"))\n"
        ));
    }
    profile
}

#[cfg(target_os = "macos")]
fn protected_entry_patterns<'a>(
    paths: impl IntoIterator<Item = &'a Path>,
) -> FleetdResult<Vec<String>> {
    let mut containing_directories = BTreeSet::new();
    for path in paths {
        if path.to_str().is_none() {
            return Err(FleetdError::InvalidConfiguration(
                "macOS worker sandbox paths must be valid UTF-8".into(),
            ));
        }
        let mut protected_ancestor = path.parent();
        while let Some(ancestor) = protected_ancestor {
            let Some(containing_directory) = ancestor.parent() else {
                break;
            };
            containing_directories.insert(containing_directory.to_path_buf());
            protected_ancestor = Some(containing_directory);
        }
    }
    containing_directories
        .into_iter()
        .map(|directory| {
            let directory = directory.to_str().ok_or_else(|| {
                FleetdError::InvalidConfiguration(
                    "macOS worker sandbox paths must be valid UTF-8".into(),
                )
            })?;
            let escaped = regex_escape(directory);
            Ok(if directory == "/" {
                "^/[^/]+$".to_string()
            } else {
                format!("^{escaped}/[^/]+$")
            })
        })
        .collect()
}

#[cfg(target_os = "macos")]
fn authority_entry_pattern(state_dir: &Path) -> FleetdResult<String> {
    let state_dir = state_dir.to_str().ok_or_else(|| {
        FleetdError::InvalidConfiguration("macOS worker sandbox paths must be valid UTF-8".into())
    })?;
    Ok(format!(
        "^{}/(fleet-state\\.json|authority\\.lock|\\.fleet-state\\.json(\\..*)?)$",
        regex_escape(state_dir)
    ))
}

#[cfg(target_os = "macos")]
fn regex_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if matches!(
            character,
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$'
        ) {
            escaped.push('\\');
        }
        escaped.push(character);
    }
    escaped
}

fn canonical_regular_file(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    let canonical = path.canonicalize().map_err(|error| {
        FleetdError::InvalidConfiguration(format!("resolving {label}: {error}"))
    })?;
    let metadata = canonical.metadata().map_err(|error| {
        FleetdError::InvalidConfiguration(format!("inspecting {label}: {error}"))
    })?;
    if !metadata.is_file() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be a regular file"
        )));
    }
    Ok(canonical)
}

async fn ensure_private_directory(path: PathBuf, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    tokio::fs::create_dir_all(&path).await?;
    let metadata = tokio::fs::symlink_metadata(&path).await?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be a real directory"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).await?;
    }
    canonical_private_directory(&path, label)
}

fn canonical_private_directory(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    let direct = std::fs::symlink_metadata(path).map_err(|error| {
        FleetdError::InvalidConfiguration(format!("inspecting {label}: {error}"))
    })?;
    if direct.file_type().is_symlink() || !direct.is_dir() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be a real directory"
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if direct.permissions().mode() & 0o077 != 0 {
            return Err(FleetdError::InvalidConfiguration(format!(
                "{label} must have mode 0700"
            )));
        }
    }
    path.canonicalize()
        .map_err(|error| FleetdError::InvalidConfiguration(format!("resolving {label}: {error}")))
}

fn canonical_future_entry(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    let parent = path.parent().ok_or_else(|| {
        FleetdError::InvalidConfiguration(format!("{label} has no containing directory"))
    })?;
    let parent = parent.canonicalize().map_err(|error| {
        FleetdError::InvalidConfiguration(format!("resolving the {label} parent: {error}"))
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| FleetdError::InvalidConfiguration(format!("{label} has no file name")))?;
    Ok(parent.join(name))
}

fn canonical_protected_root(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    let mut existing = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::symlink_metadata(existing) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_dir() {
                    return Err(FleetdError::InvalidConfiguration(format!(
                        "{label} must be a real directory or a future path below one"
                    )));
                }
                let mut canonical = existing.canonicalize().map_err(|error| {
                    FleetdError::InvalidConfiguration(format!("resolving {label}: {error}"))
                })?;
                for component in missing.into_iter().rev() {
                    canonical.push(component);
                }
                return Ok(canonical);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let name = existing.file_name().ok_or_else(|| {
                    FleetdError::InvalidConfiguration(format!(
                        "{label} has no existing directory ancestor"
                    ))
                })?;
                missing.push(name.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    FleetdError::InvalidConfiguration(format!(
                        "{label} has no existing directory ancestor"
                    ))
                })?;
            }
            Err(error) => {
                return Err(FleetdError::InvalidConfiguration(format!(
                    "inspecting {label}: {error}"
                )));
            }
        }
    }
}

fn canonical_protected_path(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    if !path.is_absolute() {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be an absolute path"
        )));
    }
    let direct = std::fs::symlink_metadata(path).map_err(|error| {
        FleetdError::InvalidConfiguration(format!("inspecting {label}: {error}"))
    })?;
    if direct.file_type().is_symlink() || !(direct.is_file() || direct.is_dir()) {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be a regular file or real directory"
        )));
    }
    #[cfg(unix)]
    if direct.is_file() {
        use std::os::unix::fs::MetadataExt as _;
        if direct.nlink() != 1 {
            return Err(FleetdError::InvalidConfiguration(format!(
                "{label} must have exactly one filesystem link"
            )));
        }
    }
    path.canonicalize()
        .map_err(|error| FleetdError::InvalidConfiguration(format!("resolving {label}: {error}")))
}

fn canonical_protected_file(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    let canonical = canonical_regular_file(path, label)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;

        let metadata = canonical.metadata().map_err(|error| {
            FleetdError::InvalidConfiguration(format!("inspecting {label}: {error}"))
        })?;
        if metadata.nlink() != 1 {
            return Err(FleetdError::InvalidConfiguration(format!(
                "{label} must have exactly one filesystem link"
            )));
        }
    }
    Ok(canonical)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_root_owned_executable(path: &Path, label: &str) -> FleetdResult<PathBuf> {
    use std::os::unix::fs::MetadataExt as _;

    let canonical = canonical_regular_file(path, label)?;
    let metadata = canonical.metadata().map_err(|error| {
        FleetdError::InvalidConfiguration(format!("inspecting {label}: {error}"))
    })?;
    if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 || metadata.mode() & 0o111 == 0 {
        return Err(FleetdError::InvalidConfiguration(format!(
            "{label} must be root-owned, executable, and not writable by group or other"
        )));
    }
    for ancestor in canonical.ancestors().skip(1) {
        let metadata = ancestor.metadata().map_err(|error| {
            FleetdError::InvalidConfiguration(format!(
                "inspecting the {label} path hierarchy: {error}"
            ))
        })?;
        if metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
            return Err(FleetdError::InvalidConfiguration(format!(
                "every directory containing {label} must be root-owned and not writable by group or other"
            )));
        }
    }
    Ok(canonical)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::time::Duration;

    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::{TcpListener, UnixListener};

    use super::*;

    #[tokio::test]
    async fn seatbelt_worker_is_confined_to_its_authority_and_session_scope() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        let token_parent = root.join("auth");
        let worker_parent = root.join("bin");
        let repo_root = root.join("repo");
        let provider_auth = root.join("provider-auth");
        let shared_state = root.join("state");
        let fleet_state = shared_state.join("fleetd");
        let worker_root = fleet_state.join("workers");
        let worker_dir = worker_root.join("worker-current");
        let peer_worker_dir = worker_root.join("worker-peer");
        let bro_root = fleet_state.join("bro");
        let bro_session_dir = bro_root.join("session-current");
        let peer_bro_dir = bro_root.join("session-peer");
        let socket_parent = fleet_state.join("run");
        let blackops_state = shared_state.join("blackopsd");
        let blackops_catalog = shared_state.join("artifacts");
        let corpus_index = root.join("corpus-index");
        for directory in [
            &token_parent,
            &worker_parent,
            &repo_root,
            &provider_auth,
            &shared_state,
            &fleet_state,
            &worker_root,
            &worker_dir,
            &peer_worker_dir,
            &bro_root,
            &bro_session_dir,
            &peer_bro_dir,
            &socket_parent,
            &blackops_state,
            &blackops_catalog,
            &corpus_index,
        ] {
            tokio::fs::create_dir_all(directory).await.unwrap();
            tokio::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
                .await
                .unwrap();
        }
        let token_path = token_parent.join("service.token");
        let worker_path = worker_parent.join("worker-probe");
        let repo_write_path = repo_root.join("repo-write-ok");
        let credential_path = provider_auth.join("auth.json");
        let authority_path = fleet_state.join("fleet-state.json");
        let peer_worker_path = peer_worker_dir.join("bootstrap.secret");
        let journal_write_path = worker_dir.join("events.jsonl");
        let peer_bro_path = peer_bro_dir.join("provider-session.json");
        let own_bro_path = bro_session_dir.join("provider-session.json");
        let socket_path = socket_parent.join("fleet.sock");
        let blackops_state_path = blackops_state.join("authority.json");
        let blackops_catalog_path = blackops_catalog.join("installed-atom.json");
        let corpus_state_path = shared_state.join("corpus-private.json");
        let corpus_index_path = corpus_index.join("meta.json");
        let worker_program = b"#!/bin/sh\nexec /bin/sh \"$@\"\n";
        tokio::fs::write(&token_path, b"fixture-service-credential\n")
            .await
            .unwrap();
        tokio::fs::write(&credential_path, b"fixture-provider-credential\n")
            .await
            .unwrap();
        tokio::fs::write(&authority_path, b"fixture-fleet-authority\n")
            .await
            .unwrap();
        tokio::fs::write(&peer_worker_path, b"fixture-peer-bootstrap\n")
            .await
            .unwrap();
        tokio::fs::write(&peer_bro_path, b"fixture-peer-provider-state\n")
            .await
            .unwrap();
        for (path, contents) in [
            (&blackops_state_path, b"fixture-blackops-state\n".as_slice()),
            (
                &blackops_catalog_path,
                b"fixture-blackops-catalog\n".as_slice(),
            ),
            (&corpus_state_path, b"fixture-corpus-state\n".as_slice()),
            (&corpus_index_path, b"fixture-corpus-index\n".as_slice()),
        ] {
            tokio::fs::write(path, contents).await.unwrap();
        }
        tokio::fs::write(&worker_path, worker_program)
            .await
            .unwrap();
        tokio::fs::set_permissions(&worker_path, std::fs::Permissions::from_mode(0o700))
            .await
            .unwrap();

        let provider_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_port = provider_listener.local_addr().unwrap().port();
        let service_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let service_port = service_listener.local_addr().unwrap().port();
        let provider = tokio::spawn(async move {
            let (mut stream, _) = provider_listener.accept().await.unwrap();
            let mut request = [0_u8; 1024];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 204 No Content\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });
        let socket_listener = UnixListener::bind(&socket_path).unwrap();
        let socket_server = tokio::spawn(async move {
            let (mut stream, _) = socket_listener.accept().await.unwrap();
            let mut request = [0_u8; 64];
            let read = stream.read(&mut request).await.unwrap();
            assert_eq!(&request[..read], b"socket-probe\n");
        });

        let sandbox = WorkerSandbox::prepare(WorkerSandboxConfig {
            external_sandbox_launcher: None,
            worker_binary: worker_path.clone(),
            service_token_file: token_path.clone(),
            fleet_state_dir: fleet_state.clone(),
            worker_root: worker_root.clone(),
            bro_root: bro_root.clone(),
            protected_peer_service_roots: vec![
                blackops_state.clone(),
                blackops_catalog.clone(),
                shared_state.clone(),
                corpus_index.clone(),
            ],
            worker_socket: socket_path.clone(),
            denied_service_ports: vec![service_port],
        })
        .await
        .unwrap();
        let mut command = sandbox
            .command(&worker_dir, &bro_session_dir, [&provider_auth])
            .unwrap();
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-c")
            .arg(
                r#"
if /bin/cat "$1" >/dev/null 2>&1; then exit 31; fi
if /usr/bin/printf overwritten >"$1" 2>/dev/null; then exit 32; fi
if /usr/bin/printf compromised >"$5" 2>/dev/null; then exit 41; fi
if /bin/cat "$6" >/dev/null 2>&1; then exit 42; fi
if /usr/bin/printf overwritten >"$6" 2>/dev/null; then exit 43; fi
if /bin/cat "$7" >/dev/null 2>&1; then exit 44; fi
if /usr/bin/printf overwritten >"$7" 2>/dev/null; then exit 45; fi
if /bin/cat "$8" >/dev/null 2>&1; then exit 46; fi
if /usr/bin/printf overwritten >"$8" 2>/dev/null; then exit 47; fi
if /bin/cat "${10}" >/dev/null 2>&1; then exit 48; fi
if /usr/bin/printf overwritten >"${10}" 2>/dev/null; then exit 49; fi
if /bin/rm "${12}" 2>/dev/null; then exit 50; fi
if /bin/mv "${12}" "${12}.moved" 2>/dev/null; then exit 51; fi
if /bin/cat "${13}" >/dev/null 2>&1; then exit 55; fi
if /usr/bin/printf overwritten >"${13}" 2>/dev/null; then exit 56; fi
if /bin/cat "${14}" >/dev/null 2>&1; then exit 57; fi
if /usr/bin/printf overwritten >"${14}" 2>/dev/null; then exit 58; fi
if /bin/cat "${15}" >/dev/null 2>&1; then exit 59; fi
if /usr/bin/printf overwritten >"${15}" 2>/dev/null; then exit 60; fi
if /bin/cat "${16}" >/dev/null 2>&1; then exit 61; fi
if /usr/bin/printf overwritten >"${16}" 2>/dev/null; then exit 62; fi
/usr/bin/printf 'socket-probe\n' | /usr/bin/nc -U -w 2 "${12}" || exit 52
/usr/bin/curl --silent --show-error --max-time 2 "http://127.0.0.1:$3/" || exit 33
if /usr/bin/curl --silent --show-error --max-time 1 --request POST --data mutation "http://127.0.0.1:$4/mutate" >/dev/null 2>&1; then exit 34; fi
/usr/bin/printf repo-write-preserved >"$2" || exit 35
/usr/bin/printf journal-write-preserved >"$9" || exit 53
/usr/bin/printf bro-session-write-preserved >"${11}" || exit 54
"#,
            )
            .arg("sandbox-probe")
            .arg(&token_path)
            .arg(&repo_write_path)
            .arg(provider_port.to_string())
            .arg(service_port.to_string())
            .arg(&worker_path)
            .arg(&credential_path)
            .arg(&authority_path)
            .arg(&peer_worker_path)
            .arg(&journal_write_path)
            .arg(&peer_bro_path)
            .arg(&own_bro_path)
            .arg(&socket_path)
            .arg(&blackops_state_path)
            .arg(&blackops_catalog_path)
            .arg(&corpus_state_path)
            .arg(&corpus_index_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null());

        let status = command.status().await.unwrap();
        assert!(status.success(), "sandbox adversary exited with {status}");
        provider.await.unwrap();
        socket_server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), service_listener.accept())
                .await
                .is_err(),
            "sandboxed worker reached the protected service listener"
        );
        assert!(
            tokio::fs::read(&token_path).await.unwrap() == b"fixture-service-credential\n",
            "sandboxed worker changed the service credential"
        );
        assert!(
            tokio::fs::read(&repo_write_path).await.unwrap() == b"repo-write-preserved",
            "sandboxed worker could not write its repository scope"
        );
        assert!(
            tokio::fs::read(&journal_write_path).await.unwrap() == b"journal-write-preserved",
            "sandboxed worker could not write its journal scope"
        );
        assert!(
            tokio::fs::read(&own_bro_path).await.unwrap() == b"bro-session-write-preserved",
            "sandboxed worker could not write its Bro session scope"
        );
        assert_eq!(
            tokio::fs::read(&credential_path).await.unwrap(),
            b"fixture-provider-credential\n"
        );
        assert_eq!(
            tokio::fs::read(&authority_path).await.unwrap(),
            b"fixture-fleet-authority\n"
        );
        assert_eq!(
            tokio::fs::read(&peer_worker_path).await.unwrap(),
            b"fixture-peer-bootstrap\n"
        );
        assert_eq!(
            tokio::fs::read(&peer_bro_path).await.unwrap(),
            b"fixture-peer-provider-state\n"
        );
        assert_eq!(
            tokio::fs::read(&blackops_state_path).await.unwrap(),
            b"fixture-blackops-state\n"
        );
        assert_eq!(
            tokio::fs::read(&blackops_catalog_path).await.unwrap(),
            b"fixture-blackops-catalog\n"
        );
        assert_eq!(
            tokio::fs::read(&corpus_state_path).await.unwrap(),
            b"fixture-corpus-state\n"
        );
        assert_eq!(
            tokio::fs::read(&corpus_index_path).await.unwrap(),
            b"fixture-corpus-index\n"
        );
        assert!(
            socket_path.exists(),
            "sandboxed worker replaced the fleet socket"
        );
        assert!(
            tokio::fs::read(&worker_path).await.unwrap() == worker_program,
            "sandboxed worker changed the trusted worker executable"
        );
    }

    #[test]
    fn seatbelt_profile_blocks_every_service_port_but_not_all_network() {
        let profile = macos_seatbelt_profile(&[7264, 7265, 7266], 2, 4, 3);
        assert!(profile.contains("(allow default)"));
        assert!(profile.contains("(subpath (param \"BRO_ROOT\"))"));
        assert!(profile.contains("(require-not (subpath (param \"BRO_SESSION_DIR\")))"));
        for index in 0..4 {
            assert!(profile.contains(&format!(
                "(subpath (param \"PROTECTED_SERVICE_ROOT_{index}\"))"
            )));
        }
        for index in 0..3 {
            assert!(profile.contains(&format!(
                "(deny file-write* (regex (param \"PROTECTED_ENTRY_PATTERN_{index}\")))"
            )));
        }
        for port in [7264, 7265, 7266] {
            // Host-agnostic `*:port` so a remote cluster corpus endpoint on that
            // port is denied too, not only the loopback tunnel form.
            assert!(profile.contains(&format!("(deny network-outbound (remote ip \"*:{port}\"))")));
            assert!(!profile.contains(&format!(
                "(deny network-outbound (remote ip \"localhost:{port}\"))"
            )));
        }
        assert!(!profile.contains("(deny network-outbound)\n"));
    }

    #[test]
    fn protected_entry_patterns_cover_each_ancestor_without_covering_descendants() {
        let patterns = protected_entry_patterns([
            Path::new("/Users/example/.local/bin/bro-harness"),
            Path::new("/Users/example/.local/state/blackbox/service.token"),
        ])
        .unwrap();
        assert!(patterns.contains(&"^/Users/example/\\.local/[^/]+$".to_string()));
        assert!(patterns.contains(&"^/Users/example/\\.local/state/[^/]+$".to_string()));
        assert!(patterns.contains(&"^/Users/example/[^/]+$".to_string()));
        assert!(patterns.contains(&"^/Users/[^/]+$".to_string()));
        assert!(patterns.contains(&"^/[^/]+$".to_string()));
        assert!(
            !patterns.contains(&"^/Users/example/\\.local/state/blackbox/[^/]+$".to_string()),
            "the literal token rule already protects the file; its parent must retain descendant writes"
        );
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn future_peer_service_roots_are_canonical_and_symlinks_fail_closed() {
        use std::os::unix::fs::symlink;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let future = root.join("not-created/catalog");
        assert_eq!(
            canonical_protected_root(&future, "peer root").unwrap(),
            future
        );

        let actual = root.join("actual");
        std::fs::create_dir(&actual).unwrap();
        let alias = root.join("alias");
        symlink(&actual, &alias).unwrap();
        assert!(matches!(
            canonical_protected_root(&alias, "peer root"),
            Err(FleetdError::InvalidConfiguration(message))
                if message.contains("real directory")
        ));
    }

    #[test]
    #[allow(clippy::disallowed_methods)]
    fn protected_files_with_preexisting_hardlinks_fail_closed() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let original = root.join("bro-harness");
        std::fs::write(&original, "worker").unwrap();
        std::fs::hard_link(&original, root.join("worker-alias")).unwrap();
        assert!(matches!(
            canonical_protected_file(&original, "worker"),
            Err(FleetdError::InvalidConfiguration(message))
                if message.contains("exactly one filesystem link")
        ));
    }
}

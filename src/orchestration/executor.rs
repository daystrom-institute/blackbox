//! The harness-worker execution seam.
//!
//! [`HarnessExecutor`] turns a fully-resolved [`WorkerSpawnSpec`] into a
//! supervised child process, exposing the stdio control/event lanes, an
//! idempotent kill, and a terminal outcome. [`LocalExecutor`] runs the child
//! in-process on the daemon host; [`super::fleetd_client::FleetdExecutor`]
//! implements the same trait over the fleetd socket without the daemon's
//! dispatch composition changing.
//!
//! Slice 5 of `design/daemon-runtime/locality-first-decomposition.md`. The
//! split is: everything below is the *execution half* the harness dispatch
//! path used to run inline in the daemon (login-shell bin resolution, env
//! hygiene, stdin control writer, stdout line pump, stderr collection, waiter
//! ordering); the daemon keeps the *state half* (task store,
//! roster/tail events, `ingest_harness_event` over the line stream,
//! terminal publication).
//!
//! There is no longer a second, inline way to start a harness worker. Every
//! dispatch path funnels through `spawn_reserved_dispatch` and arrives here,
//! which is what makes "with the fleetd executor, no harness child is a direct
//! daemon child" a structural property rather than a convention: the code that
//! could violate it does not exist.
//!
//! The seam is `async` as of the fleetd cutover: a socket-backed executor has
//! to dial, authenticate, and await a `SessionStarted` before it can hand back
//! a handle. `LocalExecutor` keeps identical behavior, with the one blocking
//! call it makes (`resolve_bin`, which shells out to a login shell) moved onto
//! `spawn_blocking` rather than run on a worker thread.

use async_trait::async_trait;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

use bro_protocol::{
    WorkerSpawnSpec, WorkerWorkspaceIdentity, WorkspaceInspectionOutcome,
    WorkspaceInspectionRequest,
};

use super::open_harness_tee;
use super::providers::{self, dispatch_prelude::ProviderExec};

/// Filesystem roots belonging to the machine that actually runs a worker.
/// `None` means the executor shares the daemon's filesystem. Remote fleetd
/// supplies explicit roots so spawn composition never leaks container-local
/// HOME/BRO_HOME paths into a worker on another machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerLocality {
    pub home: PathBuf,
    pub bro_home: PathBuf,
}

/// Host whose PATH is authoritative for the standalone harness binary.
///
/// Local execution can reject an allocator lane when the daemon cannot resolve
/// `bro-harness`. Fleetd execution must defer that check because fleetd owns
/// the worker host's login-shell PATH and performs final binary resolution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderBinaryLocation {
    #[default]
    DaemonHost,
    ExecutorHost,
}

/// Where a [`WorkerKill`] sends its one signal. The two executors reach the
/// child by different routes, but the daemon-side registry stores one type and
/// calls one method, so `cancel_task` never has to know which executor ran the
/// dispatch.
enum KillTarget {
    /// The daemon is the child's parent: signal the pid directly.
    LocalPid(Option<u32>),
    /// fleetd is the child's parent: ask it to signal. The route resolves the
    /// session's current owner connection when it is called, never at spawn,
    /// because the connection that spawned the worker may since have been
    /// replaced; a request made while no connection is up is re-sent when
    /// the session is re-adopted.
    Routed(Arc<dyn Fn() + Send + Sync>),
}

/// Idempotent kill switch for a spawned worker child. Replaces the raw
/// `child_id` PID take + `libc::kill`: `kill()` fires the signal at most once,
/// so a double-cancel (or a waiter/cancel race) is safe.
pub struct WorkerKill {
    target: KillTarget,
    fired: AtomicBool,
}

impl WorkerKill {
    fn new(pid: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            target: KillTarget::LocalPid(pid),
            fired: AtomicBool::new(false),
        })
    }

    /// A kill switch that delivers through `route` instead of a local signal.
    pub(super) fn routed(route: impl Fn() + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            target: KillTarget::Routed(Arc::new(route)),
            fired: AtomicBool::new(false),
        })
    }

    /// Terminate the child at most once. No-op once already fired, or when the
    /// local child had no pid.
    pub fn kill(&self) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        self.signal();
    }

    /// Send the termination request again even though it already fired.
    ///
    /// For a worker that is still registered after its task went terminal:
    /// the first request may never have reached it. The registry entry is
    /// removed when the worker's exit is observed, so a registered switch
    /// still names a live child.
    pub fn resend(&self) {
        self.fired.store(true, Ordering::SeqCst);
        self.signal();
    }

    fn signal(&self) {
        match &self.target {
            KillTarget::LocalPid(Some(pid)) => {
                // SAFETY: SIGTERM to a pid this daemon spawned. Matches the
                // prior `cancel_task` behavior exactly; a reused pid is the
                // same (accepted) risk the old `child_id` take carried.
                unsafe {
                    libc::kill(*pid as libc::pid_t, libc::SIGTERM);
                }
            }
            KillTarget::LocalPid(None) => {}
            KillTarget::Routed(route) => route(),
        }
    }
}

/// Terminal result of a worker: process exit code plus the full stderr the
/// executor collected across the child's lifetime.
pub struct WorkerOutcome {
    pub exit_code: Option<i32>,
    pub stderr: String,
}

/// A handle to a spawned worker. The daemon owns the state half: it registers
/// [`Self::control`] in the controls registry, ingests [`Self::events`], stores
/// [`Self::killer`]/[`Self::pid`] for cancellation/display, and awaits
/// [`Self::outcome`] to publish terminal state.
pub struct WorkerHandle {
    /// stdin control lane: NDJSON user turns / `control_request`s. The initial
    /// user turn(s) from the spec are already queued ahead of anything the
    /// daemon later sends.
    pub control: mpsc::UnboundedSender<Value>,
    /// stdout line stream: raw harness event lines for daemon-side ingest.
    /// Closes when the child's stdout reaches EOF.
    pub events: mpsc::UnboundedReceiver<String>,
    /// Child PID, for display only (kill goes through [`Self::killer`]).
    pub pid: Option<u32>,
    /// Idempotent kill switch.
    pub killer: Arc<WorkerKill>,
    /// Resolves once the child has exited and both stdio pumps have drained.
    pub outcome: oneshot::Receiver<WorkerOutcome>,
}

/// The execution seam: compose a spec centrally, hand it to an executor.
///
/// `async` because a socket-backed executor must dial, authenticate, and await
/// a `SessionStarted` acknowledgement before it can hand back a handle.
/// `#[async_trait]` rather than a native `async fn` in the trait so the daemon
/// can hold an executor behind `dyn`.
#[async_trait]
pub trait HarnessExecutor: Send + Sync {
    /// Host whose PATH decides whether the harness binary exists.
    fn provider_binary_location(&self) -> ProviderBinaryLocation {
        ProviderBinaryLocation::DaemonHost
    }

    /// Worker filesystem roots when they differ from the daemon's roots.
    fn worker_locality(&self) -> Option<&WorkerLocality> {
        None
    }

    /// Resolve worker-local managed-checkout identity before spawn. The daemon
    /// supplies the only durable scopes fleetd may accept; the executor merely
    /// verifies local filesystem and committed-Git facts.
    async fn inspect_workspace(
        &self,
        request: WorkspaceInspectionRequest,
    ) -> anyhow::Result<WorkspaceInspectionOutcome>;

    /// Spawn the worker described by `spec` and return its handle.
    async fn spawn(&self, spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle>;

    /// Begin reattaching workers that outlived the previous daemon, called
    /// once at daemon startup. Returns without waiting for the work; an
    /// executor whose workers die with the daemon has nothing to do.
    fn start_readoption(self: Arc<Self>) {}

    /// Make sure every surviving worker has been reattached to its task, so a
    /// caller deciding whether a session is live reads the task store after
    /// re-adoption rather than before it. Waits out a sweep in progress and
    /// retries one that failed; an error means re-adoption could not complete
    /// and liveness is unknown.
    async fn ensure_readopted(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Executes workers as direct child processes of the daemon on the local host.
pub struct LocalExecutor;

#[async_trait]
impl HarnessExecutor for LocalExecutor {
    async fn inspect_workspace(
        &self,
        request: WorkspaceInspectionRequest,
    ) -> anyhow::Result<WorkspaceInspectionOutcome> {
        tokio::task::spawn_blocking(move || inspect_local_workspace(request))
            .await
            .map_err(|error| anyhow::anyhow!("joining local workspace inspection: {error}"))?
    }

    async fn spawn(&self, spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle> {
        anyhow::ensure!(
            (spec.provider == bro_core::Provider::Codex) == spec.codex.is_some(),
            "Codex requires typed app-server settings"
        );
        if let Some(config) = &spec.codex {
            bro_worker::codex::validate(config)?;
        }
        let provider = spec.provider;

        // Final binary resolution stays executor-side. Login-shell resolution
        // gives the same result an interactive terminal would; a miss falls
        // back to the bare name so `Command::spawn` yields the familiar
        // "No such file or directory" error surface.
        //
        // `resolve_bin` spawns a login shell, so it goes on `spawn_blocking`
        // now that the seam is async; before the cutover the whole spawn path
        // was synchronous and this ran on the calling worker.
        let raw_bin = spec.bin_override.clone().unwrap_or_else(|| provider.bin());
        ensure_absolute_bin_executable(&raw_bin).await?;
        let bin = tokio::task::spawn_blocking({
            let raw_bin = raw_bin.clone();
            move || providers::resolve_bin(&raw_bin)
        })
        .await
        .ok()
        .flatten()
        .unwrap_or(raw_bin);

        let path_env = providers::dispatch_path_env();
        let mut cmd = Command::new(&bin);
        cmd.args(&spec.argv)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("PATH", &path_env)
            .env("NO_COLOR", "1")
            .env("TERM", "dumb")
            .env("FORCE_COLOR", "0");
        if let Some(cwd) = spec.cwd.as_deref() {
            cmd.current_dir(cwd);
        }
        // env_unset removal first (the daemon's service-env scrub list), then
        // the spec env (so it wins over any inherited value), then the pinned
        // BRO_HOME (which is on the scrub list, so it must be re-set last).
        for key in &spec.env_unset {
            cmd.env_remove(key);
        }
        for (k, v) in spec.env.iter() {
            cmd.env(k, v);
        }
        cmd.env("BRO_HOME", &spec.bro_home);

        let session_log = if spec.supervisor_writes_event_log || spec.codex.is_some() {
            Some(
                bro_worker::SessionLogWriter::open(&spec.event_log_path)
                    .await
                    .map_err(|e| anyhow::anyhow!("cannot open worker event log: {e}"))?,
            )
        } else {
            None
        };

        let mut child = cmd
            .spawn()
            .map_err(|e| anyhow::anyhow!("spawn {bin}: {e}"))?;

        let pid = child.id();
        let killer = WorkerKill::new(pid);

        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        // Control lane: queue the initial user turn(s) first, then whatever the
        // daemon later sends, all serialized to NDJSON on the writer task.
        let (control_tx, control_rx) = mpsc::unbounded_channel::<Value>();
        for msg in spec.initial_messages {
            let _ = control_tx.send(msg);
        }

        // Event lane: pump raw stdout lines out for daemon-side ingest, teeing
        // each raw line (the daemon no longer tees; it parses). A worker that
        // writes no session log of its own gets one here, at the spec's
        // pinned path, in the same `{ts, event}` shape the harness writes.
        let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
        let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
        let tee_id_out = spec.task_id.clone();
        let adapter = if let Some(config) = spec.codex.clone() {
            let stdin = stdin.ok_or_else(|| anyhow::anyhow!("Codex stdin missing"))?;
            let stdout = stdout.ok_or_else(|| anyhow::anyhow!("Codex stdout missing"))?;
            drop(stdout_done_tx);
            Some(tokio::spawn(bro_worker::codex::run(
                config,
                spec.cwd.clone().unwrap_or_else(|| ".".into()),
                stdin,
                stdout,
                control_rx,
                events_tx,
                session_log.expect("native worker log"),
            )))
        } else {
            if let Some(stdin) = stdin {
                spawn_control_writer(spec.task_id.clone(), stdin, control_rx);
            }
            if let Some(log) = session_log {
                let stdout = stdout.ok_or_else(|| anyhow::anyhow!("CLI stdout missing"))?;
                drop(stdout_done_tx);
                let mut tee = open_harness_tee(&tee_id_out, "stdout.jsonl");
                Some(tokio::spawn(bro_worker::relay_cli(
                    tokio::io::BufReader::new(stdout),
                    log,
                    provider,
                    events_tx,
                    move |line| {
                        if let Some(w) = tee.as_mut() {
                            w.try_write_line(line);
                        }
                    },
                )))
            } else {
                if let Some(stdout) = stdout {
                    tokio::spawn(async move {
                        let mut lines = tokio::io::BufReader::new(stdout).lines();
                        let mut tee = open_harness_tee(&tee_id_out, "stdout.jsonl");
                        while let Ok(Some(line)) = lines.next_line().await {
                            if let Some(w) = tee.as_mut() {
                                w.try_write_line(&line);
                            }
                            if events_tx.send(line).is_err() {
                                break;
                            }
                        }
                        let _ = stdout_done_tx.send(());
                    });
                } else {
                    let _ = stdout_done_tx.send(());
                }
                None
            }
        };

        // stderr collection: accumulate the full stream (teeing raw lines) and
        // hand it to the outcome. Mirrors the prior daemon-side accumulation,
        // except delivery is at terminal rather than incremental.
        let (stderr_done_tx, stderr_done_rx) = oneshot::channel::<String>();
        let tee_id_err = spec.task_id.clone();
        if let Some(stderr) = stderr {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stderr);
                let mut lines = reader.lines();
                let mut tee = open_harness_tee(&tee_id_err, "stderr.log");
                let mut buf = String::new();
                while let Ok(Some(line)) = lines.next_line().await {
                    if let Some(w) = tee.as_mut() {
                        w.try_write_line(&line);
                    }
                    buf.push_str(&line);
                    buf.push('\n');
                }
                let _ = stderr_done_tx.send(buf);
            });
        } else {
            let _ = stderr_done_tx.send(String::new());
        }

        // Waiter: exit, then join the stdout pump, then join stderr, then
        // publish the outcome. Same ordering the inline waiter enforced so a
        // fast fatal exit cannot race the stderr snapshot empty.
        let (outcome_tx, outcome_rx) = oneshot::channel::<WorkerOutcome>();
        tokio::spawn(async move {
            let (exit_code, adapter_error) = if let Some(adapter) = adapter {
                bro_worker::wait_for_child(child, adapter, provider.as_str()).await
            } else {
                (
                    child.wait().await.ok().and_then(|s| s.code()),
                    String::new(),
                )
            };
            let _ = stdout_done_rx.await;
            let stderr = format!(
                "{}{}",
                stderr_done_rx.await.unwrap_or_default(),
                adapter_error
            );

            let _ = outcome_tx.send(WorkerOutcome { exit_code, stderr });
        });

        Ok(WorkerHandle {
            control: control_tx,
            events: events_rx,
            pid,
            killer,
            outcome: outcome_rx,
        })
    }
}

/// Same-host implementation of the worker-local inspection contract. This is
/// intentionally beside `LocalExecutor`: dispatch composition consumes facts
/// through the executor seam regardless of which machine owns cwd.
fn inspect_local_workspace(
    request: WorkspaceInspectionRequest,
) -> anyhow::Result<WorkspaceInspectionOutcome> {
    let cwd = std::path::Path::new(&request.cwd);
    if !cwd.is_absolute() {
        return Ok(WorkspaceInspectionOutcome::Refused {
            code: "workspace.cwd_not_absolute".to_string(),
            message: "workspace cwd must be absolute".to_string(),
        });
    }
    let cwd = match cwd.canonicalize() {
        Ok(cwd) => cwd,
        Err(error) => {
            return Ok(WorkspaceInspectionOutcome::Refused {
                code: "workspace.cwd_unavailable".to_string(),
                message: format!("workspace cwd is unavailable: {error}"),
            });
        }
    };
    let Some(checkout) = bbox_corpus_core::git::managed_checkout_root(&cwd) else {
        return Ok(WorkspaceInspectionOutcome::Unmanaged);
    };

    let mut matches = request
        .candidate_scopes
        .into_iter()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter_map(|scope| {
            let project_root = if scope.bbox_root_relpath() == "." {
                checkout.clone()
            } else {
                checkout.join(scope.bbox_root_relpath())
            };
            let project_root = project_root.canonicalize().ok()?;
            if !project_root.starts_with(&checkout) || !cwd.starts_with(&project_root) {
                return None;
            }
            let config = crate::config::load_project_at_ref(&project_root, "HEAD").ok()?;
            (config.project.repo_id.as_deref() == Some(scope.repo_id()))
                .then_some((project_root.components().count(), scope))
        })
        .collect::<Vec<_>>();
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
    let raw = bbox_corpus_core::identity::ensure_checkout_id(&checkout)?;
    let workspace_id = bro_core::WorkspaceId::parse(raw)?;
    Ok(WorkspaceInspectionOutcome::Managed {
        identity: WorkerWorkspaceIdentity {
            workspace_id,
            scope,
        },
    })
}

/// The stdin control writer: serialize each `Value` as an NDJSON line to the
/// child, then close stdin when the channel drains. Relocated verbatim from the
/// daemon's former `spawn_child_control_writer`.
fn spawn_control_writer(
    task_id: String,
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<Value>,
) {
    tokio::spawn(async move {
        while let Some(input) = rx.recv().await {
            let mut line = match serde_json::to_vec(&input) {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(task_id = %task_id, %error, "failed to serialize harness input");
                    break;
                }
            };
            line.push(b'\n');
            if let Err(error) = stdin.write_all(&line).await {
                tracing::debug!(task_id = %task_id, %error, "harness child stdin closed");
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
}
/// An absolute worker binary path names one specific executable, so there is
/// nothing to resolve and nothing to fall back to: it must exist and be
/// executable on this host, or the spawn fails naming the path. "Executable"
/// means any execute bit is set: a file this user cannot execute for another
/// reason still passes here and fails at spawn.
async fn ensure_absolute_bin_executable(bin: &str) -> anyhow::Result<()> {
    if !std::path::Path::new(bin).is_absolute() {
        return Ok(());
    }
    let executable = match tokio::fs::metadata(bin).await {
        Ok(metadata) => {
            use std::os::unix::fs::PermissionsExt;
            metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
        }
        Err(_) => false,
    };
    anyhow::ensure!(
        executable,
        "harness_bin_unavailable: {bin} is missing or not executable on the executing host"
    );
    Ok(())
}

#[cfg(test)]
mod absolute_bin_tests {
    use super::ensure_absolute_bin_executable;

    async fn write_file(path: &std::path::Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::write(path, "#!/bin/sh\nexit 0\n").await.unwrap();
        tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn an_absolute_worker_binary_must_exist_and_be_executable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-harness");
        let plain = dir.path().join("not-executable");
        let runnable = dir.path().join("runnable");
        write_file(&plain, 0o644).await;
        write_file(&runnable, 0o755).await;

        for refused in [&missing, &plain, &dir.path().to_path_buf()] {
            let error = ensure_absolute_bin_executable(refused.to_str().unwrap())
                .await
                .unwrap_err()
                .to_string();
            assert!(error.starts_with("harness_bin_unavailable: "), "{error}");
            assert!(error.contains(refused.to_str().unwrap()), "{error}");
        }
        ensure_absolute_bin_executable(runnable.to_str().unwrap())
            .await
            .unwrap();
        // A bare name or relative path is resolved later, as before.
        ensure_absolute_bin_executable("bro-harness").await.unwrap();
        ensure_absolute_bin_executable("./relative/harness")
            .await
            .unwrap();
    }
}

#[cfg(test)]
mod child_env_tests {
    use super::{HarnessExecutor, LocalExecutor, WorkerSpawnSpec};

    #[tokio::test]
    async fn cli_log_failures_refuse_spawn_or_stop_the_running_child() {
        let root = tempfile::tempdir().unwrap();
        let mut spec = WorkerSpawnSpec {
            task_id: "t".into(),
            session_id: "s".into(),
            workspace_id: None,
            workspace_scope: None,
            provider: bro_core::Provider::Glm,
            bin_override: None,
            argv: vec![],
            cwd: None,
            env: Default::default(),
            env_unset: vec![],
            initial_messages: vec![],
            bro_home: root.path().into(),
            event_log_path: root.path().join("log"),
            supervisor_writes_event_log: true,
            codex: None,
        };
        spec.bin_override = Some("/bin/sh".into());
        spec.cwd = None;
        spec.supervisor_writes_event_log = true;
        spec.event_log_path = root.path().to_path_buf();
        spec.argv = vec!["-c".into(), "exit 0".into()];
        let error = match LocalExecutor.spawn(spec.clone()).await {
            Ok(_) => panic!("bad log admitted a worker"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("cannot open worker event log"));
        spec.event_log_path = root.path().join("session.events.jsonl");
        spec.argv = vec!["-c".into(), "printf 'not-json\\n'; exec sleep 60".into()];
        let child = LocalExecutor.spawn(spec).await.unwrap();
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(3), child.outcome)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.exit_code, Some(1));
        assert!(outcome.stderr.contains("worker:"), "{:?}", outcome.stderr);
    }

    /// The child environment is the daemon's minus `env_unset`, then the spec
    /// env: a variable on the scrub list is not inherited, and the same
    /// variable set by the spec reaches the child.
    #[tokio::test]
    async fn a_scrubbed_variable_reaches_the_child_only_through_the_spec_env() {
        const VAR: &str = "BRO_HARNESS_MCP_HTTP_LIFECYCLE";
        // SAFETY: nextest runs each test in its own process.
        unsafe { std::env::set_var(VAR, "inherited") };
        let seen_by_child = |env: std::collections::BTreeMap<String, String>| async move {
            let spec = WorkerSpawnSpec {
                task_id: "task-1".to_string(),
                session_id: "sess-1".to_string(),
                workspace_id: None,
                workspace_scope: None,
                provider: bro_core::Provider::Glm,
                bin_override: Some("/bin/sh".to_string()),
                argv: vec![
                    "-c".to_string(),
                    format!("printf '%s\\n' \"${{{VAR}-unset}}\""),
                ],
                cwd: None,
                env: bro_protocol::SecretEnv::new(env),
                env_unset: vec![VAR.to_string()],
                initial_messages: vec![],
                bro_home: std::env::temp_dir(),
                event_log_path: std::env::temp_dir().join("sess-1.events.jsonl"),
                supervisor_writes_event_log: false,
                codex: None,
            };
            let mut child = LocalExecutor.spawn(spec).await.unwrap();
            let line = child.events.recv().await;
            let _ = child.outcome.await;
            line
        };
        assert_eq!(
            seen_by_child(Default::default()).await.as_deref(),
            Some("unset")
        );
        let explicit = [(VAR.to_string(), "auto".to_string())].into();
        assert_eq!(seen_by_child(explicit).await.as_deref(), Some("auto"));
    }
}

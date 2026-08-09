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
//! roster/tail/system events, `ingest_harness_event` over the line stream,
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

use bro_protocol::WorkerSpawnSpec;

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

/// Where a [`WorkerKill`] sends its one signal. The two executors reach the
/// child by different routes, but the daemon-side registry stores one type and
/// calls one method, so `cancel_task` never has to know which executor ran the
/// dispatch.
enum KillTarget {
    /// The daemon is the child's parent: signal the pid directly.
    LocalPid(Option<u32>),
    /// fleetd is the child's parent: ask it to signal, over the owner
    /// connection. Delivery is best-effort by contract, exactly like the local
    /// arm (a `libc::kill` to an already-reaped pid is also a silent no-op).
    Fleetd {
        session_id: String,
        commands: tokio::sync::mpsc::UnboundedSender<bro_protocol::DaemonToFleetd>,
    },
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

    /// A kill switch that routes through fleetd instead of a local signal.
    pub(super) fn via_fleetd(
        session_id: String,
        commands: tokio::sync::mpsc::UnboundedSender<bro_protocol::DaemonToFleetd>,
    ) -> Arc<Self> {
        Arc::new(Self {
            target: KillTarget::Fleetd {
                session_id,
                commands,
            },
            fired: AtomicBool::new(false),
        })
    }

    /// Terminate the child at most once. No-op once already fired, or when the
    /// local child had no pid.
    pub fn kill(&self) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
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
            KillTarget::Fleetd {
                session_id,
                commands,
            } => {
                let _ = commands.send(bro_protocol::DaemonToFleetd::Kill {
                    session_id: session_id.clone(),
                });
            }
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
    /// Worker filesystem roots when they differ from the daemon's roots.
    fn worker_locality(&self) -> Option<&WorkerLocality> {
        None
    }

    /// Spawn the worker described by `spec` and return its handle.
    async fn spawn(&self, spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle>;
}

/// Executes workers as direct child processes of the daemon on the local host.
pub struct LocalExecutor;

#[async_trait]
impl HarnessExecutor for LocalExecutor {
    async fn spawn(&self, spec: WorkerSpawnSpec) -> anyhow::Result<WorkerHandle> {
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
        if let Some(stdin) = stdin {
            spawn_control_writer(spec.task_id.clone(), stdin, control_rx);
        }

        // Event lane: pump raw stdout lines out for daemon-side ingest, teeing
        // each raw line (the daemon no longer tees; it parses).
        let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
        let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
        let tee_id_out = spec.task_id.clone();
        if let Some(stdout) = stdout {
            tokio::spawn(async move {
                let reader = tokio::io::BufReader::new(stdout);
                let mut lines = reader.lines();
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
            let status = child.wait().await;
            let _ = stdout_done_rx.await;
            let stderr = stderr_done_rx.await.unwrap_or_default();
            let exit_code = status.ok().and_then(|s| s.code());
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

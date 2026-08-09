//! Turning a [`WorkerSpawnSpec`] into a supervised child process.
//!
//! This is a faithful port of the daemon's `LocalExecutor`
//! (`src/orchestration/executor.rs`); the parity notes live in this crate's
//! `AGENTS.md`. The ordering rules that matter, and why:
//!
//! - `env_unset` removal first (the daemon's service-env scrub list), then the
//!   spec env (so it wins over anything inherited), then `BRO_HOME` pinned
//!   LAST, because `BRO_HOME` is itself on the scrub list.
//! - `initial_messages` are queued on the control lane BEFORE the writer task
//!   starts, so the first user turn is always the first NDJSON line the child
//!   reads, ahead of anything the daemon sends later.
//! - The waiter joins the stdout pump, then stderr, and only then publishes
//!   the outcome. A fast fatal exit must not race the stderr snapshot empty.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use bro_protocol::WorkerSpawnSpec;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::{mpsc, oneshot};

/// The default harness binary name, when neither the spec nor the environment
/// names one. fleetd supervises harness workers only, so unlike the daemon's
/// per-provider `Provider::bin()` there is no provider-keyed table here.
pub const DEFAULT_HARNESS_BIN: &str = "bro-harness";

/// Cap on the stderr snapshot returned with a terminal outcome. The daemon's
/// `LocalExecutor` keeps the child's ENTIRE stderr because it hands it over an
/// in-process channel; fleetd has to fit it in a bounded RPC frame, so it
/// keeps the tail. Deliberate delta, documented in AGENTS.md.
pub const STDERR_TAIL_MAX_BYTES: usize = 64 * 1024;

/// Idempotent kill switch: `kill()` fires SIGTERM at most once, so a double
/// kill (or a kill/exit race) is safe.
#[derive(Debug)]
pub struct WorkerKill {
    pid: Option<u32>,
    fired: AtomicBool,
}

impl WorkerKill {
    /// Build a kill switch for a pid. `None` yields a switch that records the
    /// request but signals nothing (the child never got a pid).
    pub fn new(pid: Option<u32>) -> Arc<Self> {
        Arc::new(Self {
            pid,
            fired: AtomicBool::new(false),
        })
    }

    /// Send `SIGTERM` to the child at most once. No-op once already fired, or
    /// when the child never had a pid.
    pub fn kill(&self) {
        if self.fired.swap(true, Ordering::SeqCst) {
            return;
        }
        let Some(pid) = self.pid else {
            return;
        };
        // SAFETY: SIGTERM to a pid this process spawned. Same (accepted)
        // pid-reuse risk the daemon's LocalExecutor carries.
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }

    /// Whether a kill has already been requested.
    pub fn fired(&self) -> bool {
        self.fired.load(Ordering::SeqCst)
    }
}

/// Terminal result of a worker: exit code plus the bounded stderr tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerOutcome {
    pub exit_code: Option<i32>,
    pub stderr_tail: String,
}

/// The live lanes of one spawned worker.
pub struct WorkerChild {
    /// stdin control lane: NDJSON user turns / control requests. The spec's
    /// initial messages are already queued ahead of anything sent later.
    pub control: mpsc::UnboundedSender<Value>,
    /// stdout line stream: raw harness event lines, closed at child EOF.
    pub events: mpsc::UnboundedReceiver<String>,
    /// Child pid, for display and for the kill switch.
    pub pid: Option<u32>,
    /// Idempotent kill switch.
    pub killer: Arc<WorkerKill>,
    /// Resolves once the child exited AND both stdio pumps drained.
    pub outcome: oneshot::Receiver<WorkerOutcome>,
}

/// Extra PATH entries prepended for the spawned child, mirroring the daemon's
/// `dispatch_extra_path_entries`: agents follow rendered instructions to run
/// operator-local helpers that live outside a launchd/systemd PATH.
pub fn dispatch_extra_path_entries() -> Vec<PathBuf> {
    let mut entries = Vec::new();
    if let Ok(raw) = std::env::var("BRO_EXTRA_PATH") {
        entries.extend(std::env::split_paths(&raw).filter(|path| !path.as_os_str().is_empty()));
    }
    if let Ok(home) = std::env::var("HOME")
        && !home.trim().is_empty()
    {
        let home = PathBuf::from(home);
        entries.push(home.join(".local").join("bin"));
        entries.push(home.join(".cargo").join("bin"));
    }
    entries
}

/// The augmented PATH handed to spawned children.
pub fn dispatch_path_env() -> String {
    let mut entries = dispatch_extra_path_entries();
    if let Some(path) = std::env::var_os("PATH") {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries)
        .unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
        .to_string_lossy()
        .into_owned()
}

/// The raw binary name for a spec, before path resolution: the spec's
/// override, else `BRO_HARNESS_BIN`, else the default harness name.
pub fn raw_bin_for(spec: &WorkerSpawnSpec) -> String {
    spec.bin_override
        .clone()
        .or_else(|| {
            std::env::var("BRO_HARNESS_BIN")
                .ok()
                .filter(|value| !value.trim().is_empty())
        })
        .unwrap_or_else(|| DEFAULT_HARNESS_BIN.to_string())
}

/// Resolve a binary name to an absolute path using a login shell, so a CLI
/// installed under a version manager resolves the way it would in an
/// interactive terminal. BLOCKING (it spawns `bash -lc`): call it through
/// `spawn_blocking`. A miss returns `None` and the caller falls back to the
/// bare name, so `Command::spawn` yields the familiar "No such file or
/// directory" error surface rather than a silent nothing.
#[allow(clippy::disallowed_methods)]
pub fn resolve_bin(bin: &str) -> Option<String> {
    if bin.contains('/') {
        return Some(bin.to_string());
    }
    let augmented_path = dispatch_path_env();
    let output = std::process::Command::new("bash")
        .args(["-lc", &format!("command -v '{bin}'")])
        .env("PATH", &augmented_path)
        .output()
        .ok();
    if let Some(output) = output
        && output.status.success()
        && let Ok(stdout) = String::from_utf8(output.stdout)
    {
        let path = stdout.trim().to_string();
        if !path.is_empty() {
            return Some(path);
        }
    }
    // A Debian login shell's /etc/profile plainly reassigns PATH, clobbering
    // the augmented env above, so a shell miss falls back to walking the
    // augmented PATH directly.
    find_in_path_env(bin, &augmented_path)
}

#[allow(clippy::disallowed_methods)]
fn find_in_path_env(bin: &str, path_env: &str) -> Option<String> {
    for dir in std::env::split_paths(path_env) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let candidate = dir.join(bin);
        let Ok(metadata) = std::fs::metadata(&candidate) else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Some(candidate.to_string_lossy().into_owned());
    }
    None
}

/// The top-level `seq` of a raw harness stdout envelope line, when it carries
/// one. fleetd never invents or renumbers sequence values: a pre-seq harness
/// build simply relays `None` and the daemon falls back to its own ordering.
pub fn event_seq(line: &str) -> Option<u64> {
    serde_json::from_str::<Value>(line)
        .ok()?
        .get("seq")?
        .as_u64()
}

/// Spawn the worker described by `spec` and return its live lanes.
pub async fn spawn_worker(spec: WorkerSpawnSpec) -> anyhow::Result<WorkerChild> {
    let raw_bin = raw_bin_for(&spec);
    // Login-shell resolution shells out, so it must not block the reactor.
    let resolve_target = raw_bin.clone();
    let bin = tokio::task::spawn_blocking(move || resolve_bin(&resolve_target))
        .await
        .unwrap_or(None)
        .unwrap_or(raw_bin);

    let mut command = Command::new(&bin);
    command
        .args(&spec.argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PATH", dispatch_path_env())
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("FORCE_COLOR", "0");
    if let Some(cwd) = spec.cwd.as_deref() {
        command.current_dir(cwd);
    }
    // Scrub list first, spec env second (so it wins), BRO_HOME last (it is on
    // the scrub list, so it must be re-set after the removals).
    for key in &spec.env_unset {
        command.env_remove(key);
    }
    for (key, value) in spec.env.iter() {
        command.env(key, value);
    }
    command.env("BRO_HOME", &spec.bro_home);

    let mut child = command
        .spawn()
        .map_err(|error| anyhow::anyhow!("spawn {bin}: {error}"))?;

    let pid = child.id();
    let killer = WorkerKill::new(pid);

    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Control lane: the spec's initial turns are queued before the writer
    // task exists, so they are unconditionally first on the wire.
    let (control_tx, control_rx) = mpsc::unbounded_channel::<Value>();
    for message in spec.initial_messages {
        let _ = control_tx.send(message);
    }
    if let Some(stdin) = stdin {
        spawn_control_writer(spec.session_id.clone(), stdin, control_rx);
    }

    // Event lane: raw stdout lines out for relay. No tee here: teeing is a
    // daemon-side transcript concern, and the harness child already writes
    // its own durable event log under the spec's BRO_HOME.
    let (events_tx, events_rx) = mpsc::unbounded_channel::<String>();
    let (stdout_done_tx, stdout_done_rx) = oneshot::channel::<()>();
    if let Some(stdout) = stdout {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                if events_tx.send(line).is_err() {
                    break;
                }
            }
            let _ = stdout_done_tx.send(());
        });
    } else {
        let _ = stdout_done_tx.send(());
    }

    // stderr: accumulate a bounded tail for the terminal outcome.
    let (stderr_done_tx, stderr_done_rx) = oneshot::channel::<String>();
    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            let mut lines = tokio::io::BufReader::new(stderr).lines();
            let mut tail = StderrTail::new(STDERR_TAIL_MAX_BYTES);
            while let Ok(Some(line)) = lines.next_line().await {
                tail.push_line(&line);
            }
            let _ = stderr_done_tx.send(tail.into_string());
        });
    } else {
        let _ = stderr_done_tx.send(String::new());
    }

    // Waiter: exit, then join stdout, then join stderr, then publish. Same
    // ordering LocalExecutor enforces.
    let (outcome_tx, outcome_rx) = oneshot::channel::<WorkerOutcome>();
    tokio::spawn(async move {
        let status = child.wait().await;
        let _ = stdout_done_rx.await;
        let stderr_tail = stderr_done_rx.await.unwrap_or_default();
        let exit_code = status.ok().and_then(|status| status.code());
        let _ = outcome_tx.send(WorkerOutcome {
            exit_code,
            stderr_tail,
        });
    });

    Ok(WorkerChild {
        control: control_tx,
        events: events_rx,
        pid,
        killer,
        outcome: outcome_rx,
    })
}

/// Serialize each control message as one NDJSON line to the child's stdin,
/// then close stdin when the channel drains.
fn spawn_control_writer(
    session_id: String,
    mut stdin: tokio::process::ChildStdin,
    mut rx: mpsc::UnboundedReceiver<Value>,
) {
    tokio::spawn(async move {
        while let Some(input) = rx.recv().await {
            let mut line = match serde_json::to_vec(&input) {
                Ok(line) => line,
                Err(error) => {
                    tracing::warn!(%session_id, %error, "failed to serialize harness input");
                    break;
                }
            };
            line.push(b'\n');
            if let Err(error) = stdin.write_all(&line).await {
                tracing::debug!(%session_id, %error, "harness child stdin closed");
                break;
            }
        }
        let _ = stdin.shutdown().await;
    });
}

/// A bounded, line-granular stderr tail. Whole lines are dropped from the
/// front rather than bytes, so the snapshot is never a half-line and never
/// splits a UTF-8 sequence.
#[derive(Debug)]
pub struct StderrTail {
    lines: VecDeque<String>,
    bytes: usize,
    max_bytes: usize,
}

impl StderrTail {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            bytes: 0,
            max_bytes,
        }
    }

    pub fn push_line(&mut self, line: &str) {
        let cost = line.len() + 1;
        self.lines.push_back(line.to_string());
        self.bytes += cost;
        while self.bytes > self.max_bytes && self.lines.len() > 1 {
            if let Some(dropped) = self.lines.pop_front() {
                self.bytes -= dropped.len() + 1;
            }
        }
    }

    pub fn into_string(self) -> String {
        let mut out = String::with_capacity(self.bytes);
        for line in self.lines {
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_seq_reads_the_top_level_field_only() {
        assert_eq!(event_seq(r#"{"type":"assistant","seq":12}"#), Some(12));
        assert_eq!(event_seq(r#"{"type":"assistant"}"#), None);
        // A nested seq is not the envelope's own seq.
        assert_eq!(event_seq(r#"{"type":"x","event":{"seq":9}}"#), None);
        // Malformed and non-object lines never panic.
        assert_eq!(event_seq("not json"), None);
        assert_eq!(event_seq("[1,2,3]"), None);
        assert_eq!(event_seq(r#"{"seq":"12"}"#), None);
    }

    #[test]
    fn stderr_tail_keeps_the_tail_and_stays_bounded() {
        let mut tail = StderrTail::new(32);
        for index in 0..100 {
            tail.push_line(&format!("line-{index}"));
        }
        let text = tail.into_string();
        assert!(
            text.len() <= 32,
            "tail must stay bounded: {} bytes",
            text.len()
        );
        assert!(text.contains("line-99"), "tail must keep the newest line");
        assert!(!text.contains("line-0\n"), "oldest lines are dropped");
        assert!(text.ends_with('\n'));
    }

    /// A single line longer than the cap is kept rather than producing an
    /// empty snapshot: one over-long line is still the most useful thing we
    /// have about why a child died.
    #[test]
    fn stderr_tail_keeps_one_oversize_line() {
        let mut tail = StderrTail::new(8);
        tail.push_line(&"x".repeat(100));
        assert_eq!(tail.into_string().trim_end().len(), 100);
    }

    #[test]
    fn stderr_tail_never_splits_a_utf8_sequence() {
        let mut tail = StderrTail::new(16);
        for _ in 0..20 {
            tail.push_line("héllo wörld ✅");
        }
        // into_string is String-typed; the assertion is that we got here
        // without a panic and the content is whole lines.
        let text = tail.into_string();
        for line in text.lines() {
            assert_eq!(line, "héllo wörld ✅");
        }
    }

    #[test]
    fn raw_bin_prefers_the_spec_override() {
        let mut spec = sample_spec();
        spec.bin_override = Some("/opt/custom-harness".to_string());
        assert_eq!(raw_bin_for(&spec), "/opt/custom-harness");
    }

    /// With no override and no env, the default harness name is used and
    /// `Command::spawn` produces the familiar not-found error.
    #[test]
    fn raw_bin_falls_back_to_the_default_name() {
        let mut spec = sample_spec();
        spec.bin_override = None;
        // BRO_HARNESS_BIN is not set in the nextest process for this test;
        // nextest is process-per-test, so this does not race a sibling.
        if std::env::var("BRO_HARNESS_BIN").is_err() {
            assert_eq!(raw_bin_for(&spec), DEFAULT_HARNESS_BIN);
        }
    }

    #[test]
    fn resolve_bin_passes_through_explicit_paths() {
        assert_eq!(
            resolve_bin("/opt/custom/bro-harness").as_deref(),
            Some("/opt/custom/bro-harness")
        );
    }

    #[test]
    fn kill_switch_is_idempotent() {
        let killer = WorkerKill::new(None);
        assert!(!killer.fired());
        killer.kill();
        assert!(killer.fired());
        // Second kill is a no-op, not a double signal.
        killer.kill();
        assert!(killer.fired());
    }

    fn sample_spec() -> WorkerSpawnSpec {
        WorkerSpawnSpec {
            task_id: "task-1".to_string(),
            session_id: "sess-1".to_string(),
            workspace_id: None,
            workspace_scope: None,
            provider: bro_core::Provider::Glm,
            bin_override: None,
            argv: vec![],
            cwd: None,
            env: Default::default(),
            env_unset: vec![],
            initial_messages: vec![],
            bro_home: PathBuf::from("/state/bro"),
            event_log_path: PathBuf::from("/state/bro/sess-1.events.jsonl"),
        }
    }
}

//! Long-running shell support: `shell_run` (spawn + cooperative yield),
//! `shell_poll` (drain / feed / await an existing session), and `shell_kill`
//! (signal + reap a session).
//!
//! Model mirrors Codex's `exec_command` / `write_stdin`: a command that
//! finishes within `yield_time_ms` returns its full result inline; one that
//! doesn't returns a `session_id` + partial output, and `shell_poll` resumes
//! draining it (and may feed more stdin). This is the cooperative middle path
//! between block-forever and a full background task registry — no wake
//! machinery, synchronous from the agent loop's view. See
//! design/bro-harness/bro-harness-tool-surface.md.
//!
//! Sessions are in-memory and live only within a single harness `run()` (across
//! the LLM turns of one dispatch, NOT across exec → resume): a live OS child
//! can't be serialized into the persisted `side` cell. Children are spawned
//! `kill_on_drop` in their own process group, so abandoned sessions die — whole
//! group, grandchildren included — when the `ToolCx` drops
//! ([`ShellSession`]'s `Drop` signals the group; `kill_on_drop` reaps the
//! direct child).

use crate::promise::{PromiseProgress, StreamKind};
use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use regex::Regex;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Per-stream in-memory buffer cap. Beyond this, output is counted-and-dropped
/// rather than retained, so a runaway producer can't exhaust memory.
const MAX_BUF_BYTES: usize = 8 * 1024 * 1024;
/// Default returned-output budget (~40 KB at a 4-bytes/token heuristic).
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
/// Default cooperative yield for a fresh command. Long commands should not make
/// the whole agent turn look hung just because the model forgot to set
/// `yield_time_ms`.
const DEFAULT_RUN_YIELD_MS: u64 = 1_000;
/// Default cooperative yield when polling an already-yielded command. The poll
/// default is longer because the model is explicitly checking an active child.
const DEFAULT_POLL_YIELD_MS: u64 = 5_000;
/// Max concurrently-retained (still-running) sessions per dispatch. A blocking
/// command never counts; only yielded sessions are retained. Prevents a loop
/// from accumulating unbounded live children.
const MAX_LIVE_SESSIONS: usize = 32;
/// Grace window for readers to flush final bytes after a child exits, before we
/// abort them. Bounds the case where a grandchild inherited the pipe and holds
/// it open past the direct child's exit (which would otherwise hang forever).
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A drained-into output buffer with an overflow counter.
#[derive(Default)]
struct OutBuf {
    bytes: Vec<u8>,
    dropped: usize,
}

impl OutBuf {
    fn push(&mut self, chunk: &[u8]) {
        let room = MAX_BUF_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() <= room {
            self.bytes.extend_from_slice(chunk);
        } else {
            self.bytes.extend_from_slice(&chunk[..room]);
            self.dropped += chunk.len() - room;
        }
    }
    /// Drain accumulated output as lossy UTF-8; returns (text, dropped) and
    /// resets both.
    fn take(&mut self) -> (String, usize) {
        let s = String::from_utf8_lossy(&self.bytes).into_owned();
        let dropped = self.dropped;
        self.bytes.clear();
        self.dropped = 0;
        (s, dropped)
    }
}

/// One live child plus the background readers draining its pipes.
struct ShellSession {
    child: Child,
    /// `Some` while stdin is open for feeding; `None` once closed (EOF sent).
    stdin: Option<ChildStdin>,
    stdout: Arc<Mutex<OutBuf>>,
    stderr: Arc<Mutex<OutBuf>>,
    readers: Vec<JoinHandle<()>>,
    /// Absolute hard-kill deadline carried from the originating `shell_run`'s
    /// `timeout_ms`, so polls honor the same ceiling.
    kill_at: Option<Instant>,
    /// The command line, retained so `shell_list` can identify orphanable
    /// sessions whose id was lost.
    command: String,
    /// When the session was spawned, for an elapsed readout in `shell_list`.
    started: Instant,
    /// Shared progress for a yielded session — the readers heartbeat into this
    /// so `shell_poll`/`shell_list` can expose running-progress metadata.
    progress: Option<Arc<PromiseProgress>>,
    /// Optional post-capture output filter. This never affects process exit
    /// status; it only reduces returned stdout/stderr lines after capture.
    output_filter: Option<ShellOutputFilter>,
}

impl Drop for ShellSession {
    /// `kill_on_drop` only covers the direct bash child; an abandoned live
    /// session (the `ToolCx` drops mid-run) must take its whole process group
    /// down too, or grandchildren keep running. `Child::id()` returns `None`
    /// once the child has been reaped, so this never fires for a command that
    /// already exited — survivors a *successful* command intentionally
    /// backgrounded are left alone, same as before.
    fn drop(&mut self) {
        if let Some(pid) = self.child.id() {
            signal_group(pid, libc::SIGKILL);
        }
    }
}

/// Session table hung off `ToolCx`. In-memory, single-`run()` lifetime.
#[derive(Default)]
pub struct ShellSessions {
    map: HashMap<String, ShellSession>,
    counter: u64,
}

impl ShellSessions {
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn ids(&self) -> Vec<String> {
        self.map.keys().cloned().collect()
    }

    pub fn shutdown_all(&mut self) -> usize {
        let count = self.map.len();
        self.map.clear();
        count
    }

    /// Insert a retained session, or hand it back (boxed; the session is large)
    /// if at the live cap so the caller can kill it and surface an error.
    fn insert(&mut self, s: ShellSession) -> Result<String, Box<ShellSession>> {
        if self.map.len() >= MAX_LIVE_SESSIONS {
            return Err(Box::new(s));
        }
        self.counter += 1;
        let id = format!("sh-{}", self.counter);
        self.map.insert(id.clone(), s);
        Ok(id)
    }
}

enum Outcome {
    Exited(Option<i32>),
    Yielded,
    TimedOut,
}

/// Running-progress metadata for a yielded session, drawn from the same
/// [`PromiseProgress`] the pipe readers heartbeat into. Surfaces elapsed
/// runtime, last-output recency, and stdout/stderr byte counts so an agent
/// polling a silent-but-healthy long command (a quiet `cargo build`) sees it is
/// still making progress.
fn session_progress(session: &ShellSession) -> Option<Value> {
    let progress = session.progress.as_ref()?;
    let started_ms =
        crate::promise::now_ms().saturating_sub(session.started.elapsed().as_millis() as u64);
    Some(progress.snapshot(started_ms))
}

/// Convert a model-requested wait window into a deadline. `0` means no yield
/// deadline, so explicit long waits are honored all the way to exit or timeout.
/// There is intentionally no low safety cap here: the model-facing contract lets
/// agents request 60-180s waits to avoid extra polling turns, while `timeout_ms`
/// remains the hard-kill safety boundary for runaway children.
fn yield_deadline(now: Instant, requested_ms: Option<u64>, default_ms: u64) -> Option<Instant> {
    let yield_ms = requested_ms.unwrap_or(default_ms);
    (yield_ms > 0).then(|| now + Duration::from_millis(yield_ms))
}

/// Drive a child until it exits, the yield deadline elapses, or the hard-kill
/// deadline elapses (in which case its whole process group is killed). Holds
/// no lock.
async fn drive(child: &mut Child, yield_at: Option<Instant>, kill_at: Option<Instant>) -> Outcome {
    let far = Instant::now() + Duration::from_secs(31_536_000);
    let y = yield_at.unwrap_or(far);
    let k = kill_at.unwrap_or(far);
    tokio::select! {
        s = child.wait() => Outcome::Exited(s.ok().and_then(|st| st.code())),
        _ = sleep_until(y), if yield_at.is_some() => Outcome::Yielded,
        _ = sleep_until(k), if kill_at.is_some() => {
            // Group-wide SIGKILL first: a single-pid kill leaves grandchildren
            // (sccache/rustc under a timed-out cargo) alive holding e.g. the
            // target-dir build lock. Then reap the direct child.
            if let Some(pid) = child.id() {
                signal_group(pid, libc::SIGKILL);
            }
            let _ = child.kill().await;
            Outcome::TimedOut
        }
    }
}

fn spawn_reader<R>(
    mut r: R,
    buf: Arc<Mutex<OutBuf>>,
    progress: Option<(StreamKind, Arc<PromiseProgress>)>,
) -> JoinHandle<()>
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.lock().unwrap().push(&chunk[..n]);
                    if let Some((kind, ref p)) = progress {
                        p.heartbeat(kind, n);
                    }
                }
            }
        }
    })
}

/// Keep the last `budget` bytes (errors trail in shell output), adjusted to a
/// char boundary. Returns (tail, bytes_dropped_from_head).
fn cap_tail(s: &str, budget: usize) -> (String, usize) {
    if s.len() <= budget {
        return (s.to_string(), 0);
    }
    let mut start = s.len() - budget;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    (s[start..].to_string(), start)
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
struct ShellOutputFilterInput {
    /// Keep only stdout lines matching one of these regexes.
    #[serde(default, deserialize_with = "deserialize_filter_patterns")]
    #[schemars(with = "ShellOutputFilterPatternsSchema")]
    stdout: Vec<String>,
    /// Keep only stderr lines matching one of these regexes.
    #[serde(default, deserialize_with = "deserialize_filter_patterns")]
    #[schemars(with = "ShellOutputFilterPatternsSchema")]
    stderr: Vec<String>,
}

#[derive(JsonSchema)]
#[schemars(untagged)]
#[allow(dead_code)]
enum ShellOutputFilterPatternsSchema {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum ShellOutputFilterPatternsInput {
    One(String),
    Many(Vec<String>),
}

fn deserialize_filter_patterns<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    match ShellOutputFilterPatternsInput::deserialize(deserializer)? {
        ShellOutputFilterPatternsInput::One(pattern) => Ok(vec![pattern]),
        ShellOutputFilterPatternsInput::Many(patterns) => Ok(patterns),
    }
}

#[derive(Clone)]
struct ShellOutputFilter {
    stdout_patterns: Vec<String>,
    stderr_patterns: Vec<String>,
    stdout: Vec<Regex>,
    stderr: Vec<Regex>,
}

struct FilteredStream {
    text: String,
    report: Option<Value>,
}

struct ShellOutputSnapshot {
    stdout: String,
    stderr: String,
    output_filter: Option<Value>,
}

fn compile_output_filter(
    input: Option<ShellOutputFilterInput>,
) -> Result<Option<ShellOutputFilter>, String> {
    let Some(input) = input else {
        return Ok(None);
    };
    if input.stdout.is_empty() && input.stderr.is_empty() {
        return Ok(None);
    }
    let stdout = compile_filter_patterns("stdout", &input.stdout)?;
    let stderr = compile_filter_patterns("stderr", &input.stderr)?;
    Ok(Some(ShellOutputFilter {
        stdout_patterns: input.stdout,
        stderr_patterns: input.stderr,
        stdout,
        stderr,
    }))
}

fn compile_filter_patterns(stream: &str, patterns: &[String]) -> Result<Vec<Regex>, String> {
    const MAX_PATTERNS: usize = 32;
    const MAX_PATTERN_BYTES: usize = 512;
    if patterns.len() > MAX_PATTERNS {
        return Err(format!(
            "output_filter.{stream}: at most {MAX_PATTERNS} patterns are supported"
        ));
    }
    patterns
        .iter()
        .map(|pattern| {
            if pattern.len() > MAX_PATTERN_BYTES {
                return Err(format!(
                    "output_filter.{stream}: pattern is too long ({} bytes > {MAX_PATTERN_BYTES})",
                    pattern.len()
                ));
            }
            Regex::new(pattern)
                .map_err(|e| format!("output_filter.{stream}: invalid regex `{pattern}`: {e}"))
        })
        .collect()
}

fn filter_stream(raw: String, patterns: &[Regex], pattern_text: &[String]) -> FilteredStream {
    if patterns.is_empty() {
        return FilteredStream {
            text: raw,
            report: None,
        };
    }
    let mut kept = String::new();
    let mut kept_lines = 0usize;
    let mut dropped_lines = 0usize;
    for line in raw.split_inclusive('\n') {
        if patterns.iter().any(|pattern| pattern.is_match(line)) {
            kept.push_str(line);
            kept_lines += 1;
        } else {
            dropped_lines += 1;
        }
    }
    FilteredStream {
        text: kept,
        report: Some(json!({
            "mode": "matching_lines",
            "patterns": pattern_text,
            "kept_lines": kept_lines,
            "dropped_lines": dropped_lines,
        })),
    }
}

fn render(raw: String, dropped: usize, max_tokens: usize) -> String {
    let budget = max_tokens.saturating_mul(4).max(1);
    let (body, head_trunc) = cap_tail(&raw, budget);
    let mut prefix = String::new();
    if head_trunc > 0 {
        prefix.push_str(&format!("[... {head_trunc} earlier bytes truncated]\n"));
    }
    if dropped > 0 {
        prefix.push_str(&format!("[... {dropped} bytes dropped at buffer cap]\n"));
    }
    format!("{prefix}{body}")
}

/// Snapshot both buffers (draining them) and render with the token budget.
fn snapshot(session: &ShellSession, max_tokens: usize) -> ShellOutputSnapshot {
    let (so, so_drop) = session.stdout.lock().unwrap().take();
    let (se, se_drop) = session.stderr.lock().unwrap().take();
    render_snapshot(
        so,
        so_drop,
        se,
        se_drop,
        session.output_filter.as_ref(),
        max_tokens,
    )
}

fn render_snapshot(
    stdout: String,
    stdout_dropped: usize,
    stderr: String,
    stderr_dropped: usize,
    filter: Option<&ShellOutputFilter>,
    max_tokens: usize,
) -> ShellOutputSnapshot {
    let (stdout, stderr, output_filter) = if let Some(filter) = filter {
        let stdout = filter_stream(stdout, &filter.stdout, &filter.stdout_patterns);
        let stderr = filter_stream(stderr, &filter.stderr, &filter.stderr_patterns);
        let mut report = serde_json::Map::new();
        if let Some(stdout_report) = stdout.report {
            report.insert("stdout".to_string(), stdout_report);
        }
        if let Some(stderr_report) = stderr.report {
            report.insert("stderr".to_string(), stderr_report);
        }
        let report = (!report.is_empty()).then_some(Value::Object(report));
        (stdout.text, stderr.text, report)
    } else {
        (stdout, stderr, None)
    };
    ShellOutputSnapshot {
        stdout: render(stdout, stdout_dropped, max_tokens),
        stderr: render(stderr, stderr_dropped, max_tokens),
        output_filter,
    }
}

/// After a child has exited (or been killed) give its readers a bounded grace
/// window to flush remaining bytes, then ABORT stragglers and drain.
///
/// The bound is load-bearing: when a command backgrounds a process that
/// inherited the stdout/stderr pipe (`cmd &`), the direct child exits but the
/// pipe stays open, so a reader awaiting EOF would block forever. We abort it
/// instead of hanging the agent loop.
async fn drain_final(session: &mut ShellSession, max_tokens: usize) -> ShellOutputSnapshot {
    let readers = std::mem::take(&mut session.readers);
    let aborts: Vec<_> = readers.iter().map(|h| h.abort_handle()).collect();
    let join_all = async move {
        for h in readers {
            let _ = h.await;
        }
    };
    if tokio::time::timeout(READER_DRAIN_GRACE, join_all)
        .await
        .is_err()
    {
        for a in aborts {
            a.abort();
        }
    }
    snapshot(session, max_tokens)
}

/// Map a signal name to (libc signal, canonical name). Unknown → SIGTERM.
fn signal_for(name: Option<&str>) -> (i32, &'static str) {
    match name {
        Some("kill") => (libc::SIGKILL, "kill"),
        Some("int") => (libc::SIGINT, "int"),
        _ => (libc::SIGTERM, "term"),
    }
}

/// Send `sig` to the child's whole process group. The child is spawned as its
/// own group leader (`process_group(0)`), so `-pid` addresses the group —
/// grandchildren included. Mirrors codex-rs's killpg-based group cleanup
/// (codex-rs/utils/pty/src/process_group.rs).
fn signal_group(pid: u32, sig: i32) {
    // SAFETY: kill(2) with a constant signal; a negative pid targets the
    // process group. ESRCH on an already-dead group is harmless.
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

/// Send `sig` to a session's process group (no-op if already reaped).
fn signal_child(session: &ShellSession, sig: i32) {
    if let Some(pid) = session.child.id() {
        signal_group(pid, sig);
    }
}

/// Build the terminal JSON for an exited/timed-out/killed session.
fn terminal_json(exit_code: Option<i32>, output: ShellOutputSnapshot, timed_out: bool) -> Value {
    let mut out = json!({
        "exit_code": exit_code,
        "stdout": output.stdout,
        "stderr": output.stderr,
        "running": false,
        "timed_out": timed_out,
    });
    if let Some(report) = output.output_filter {
        out["output_filter"] = report;
    }
    out
}

/// Build a terminal `shell_run` result.
fn terminal_result(
    exit_code: Option<i32>,
    output: ShellOutputSnapshot,
    timed_out: bool,
) -> ToolResult {
    ToolResult::Json(terminal_json(exit_code, output, timed_out))
}

fn shell_path_env() -> Option<OsString> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    augmented_path_env(
        std::env::var_os("BRO_EXTRA_PATH"),
        home,
        std::env::var_os("PATH"),
    )
}

tokio::task_local! {
    /// Env var names to scrub from spawned child processes — bound by an
    /// in-process host (the daemon) so its own service config does not leak
    /// into the user shell commands an agent runs. Unbound for the standalone
    /// binary, where there's nothing host-internal to hide.
    static SPAWN_SCRUB: std::sync::Arc<Vec<String>>;
}

/// Run `fut` with a child-process env scrub list bound. Spawned shell children
/// get these env vars removed (harness-daemon-boundary.md §3) — the in-process
/// replacement for the daemon's old "remove service env, restore after" dance
/// that forced sessions to serialize under a lock.
pub async fn with_spawn_scrub<F>(keys: Vec<String>, fut: F) -> F::Output
where
    F: std::future::Future,
{
    SPAWN_SCRUB.scope(std::sync::Arc::new(keys), fut).await
}

/// Apply the standard child-process environment for a non-interactive shell
/// command: augmented PATH, clean/uncolored output, and the host scrub set.
/// Per-command `args.env` is layered on top by the caller and wins.
fn apply_child_env(
    cmd: &mut tokio::process::Command,
    shell_env: &std::collections::BTreeMap<String, String>,
) {
    // Non-interactive execution: deterministic, uncolored output for the model.
    cmd.env("NO_COLOR", "1");
    cmd.env("FORCE_COLOR", "0");
    cmd.env("TERM", "dumb");
    if let Some(path) = shell_path_env() {
        cmd.env("PATH", path);
    }
    // No-op outside a with_spawn_scrub scope (the standalone binary).
    let _ = SPAWN_SCRUB.try_with(|keys| {
        for k in keys.iter() {
            cmd.env_remove(k);
        }
    });
    // Host-supplied non-secret overlay (ToolCx::shell_env), applied after the
    // scrub so an explicit host choice is never scrubbed away. Callers apply
    // the model's per-call `env` after this, so the model still wins.
    for (k, v) in shell_env {
        cmd.env(k, v);
    }
}

fn augmented_path_env(
    extra_path: Option<OsString>,
    home: Option<PathBuf>,
    current_path: Option<OsString>,
) -> Option<OsString> {
    let mut entries = Vec::new();
    if let Some(raw) = extra_path {
        entries.extend(std::env::split_paths(&raw).filter(|path| !path.as_os_str().is_empty()));
    }
    if let Some(home) = home {
        entries.push(home.join(".local").join("bin"));
        entries.push(home.join(".cargo").join("bin"));
    }
    if let Some(path) = current_path {
        entries.extend(std::env::split_paths(&path));
    }
    std::env::join_paths(entries).ok()
}

// ---------------------------------------------------------------------------
// shell_run
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellRunInput {
    /// The shell command line to execute (run via `bash -lc`).
    command: String,
    /// Working subdirectory relative to the worktree root.
    cwd: Option<String>,
    /// Hard-kill deadline in milliseconds. When set, the process is killed if
    /// it runs longer than this (honored across subsequent shell_poll calls).
    timeout_ms: Option<u64>,
    /// Cooperative yield in milliseconds. If the command has not finished by
    /// then, returns partial output plus a `session_id` (the process keeps
    /// running); resume with shell_poll. When omitted, defaults to a short
    /// cooperative yield so long commands do not stall the agent loop. Set to
    /// 0 only when you deliberately want to block until completion/timeout.
    yield_time_ms: Option<u64>,
    /// Cap on returned stdout/stderr, in approximate tokens (~4 bytes each;
    /// default 10000). The TAIL is kept so trailing errors survive.
    max_output_tokens: Option<usize>,
    /// Initial stdin written to the process. The stream stays open for
    /// shell_poll to feed more, unless close_stdin is set.
    stdin: Option<String>,
    /// Close (EOF) the stdin stream after writing `stdin`. Required for
    /// commands that read until EOF (e.g. `cat`, `sort`) to terminate.
    #[serde(default)]
    close_stdin: bool,
    /// Extra environment variables for the process, merged onto the inherited
    /// environment (these win on conflict). Cleaner than inlining `FOO=bar` in
    /// the command for things like `PORT`, `RUST_LOG`, etc.
    #[serde(default)]
    env: HashMap<String, String>,
    /// Optional post-capture line filter. Patterns are regexes; matching lines
    /// are kept and non-matching lines are dropped from the returned stream.
    /// The child process is not wrapped, so exit_code remains the real command
    /// exit status.
    output_filter: Option<ShellOutputFilterInput>,
}

pub struct ShellRun;

#[async_trait]
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }
    fn description(&self) -> &str {
        "Run a shell command in the worktree (bash -lc). Returns {exit_code, stdout, stderr, running, timed_out}. Long commands yield by default after ~1s with running=true + session_id; set yield_time_ms to wait that many ms for exit, or 0 to block until exit/timeout. Continue yielded sessions with shell_poll until running=false. timeout_ms hard-kills a runaway; max_output_tokens caps output (tail kept). output_filter keeps matching stdout/stderr lines after capture without changing the real exit_code. stdin feeds initial input; close_stdin sends EOF; env injects variables. Refuses categorically destructive commands."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellRunInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellRunInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        if let Some(reason) = cx.safety.deny_command(&args.command) {
            return ToolResult::Error(format!("refused: {reason}"));
        }
        let output_filter = match compile_output_filter(args.output_filter) {
            Ok(filter) => filter,
            Err(e) => return ToolResult::Error(e),
        };
        let cwd =
            match crate::workspace::resolve_in_root(&cx.root, args.cwd.as_deref().unwrap_or(".")) {
                Ok(p) => p,
                Err(e) => return ToolResult::Error(e.to_string()),
            };
        let max_tokens = args.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &args.command])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            // Each command gets its own process group so kill/timeout paths
            // can take down the whole tree with one negative-pid kill(2)
            // (codex-rs does the equivalent via setsid/setpgid in pre_exec).
            // kill_on_drop alone only reaps the direct bash child.
            .process_group(0);
        apply_child_env(&mut cmd, &cx.shell_env);
        for (k, v) in &args.env {
            cmd.env(k, v);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return ToolResult::Error(format!("spawn failed: {e}")),
        };

        let stdout = Arc::new(Mutex::new(OutBuf::default()));
        let stderr = Arc::new(Mutex::new(OutBuf::default()));
        let progress = Arc::new(PromiseProgress::new());
        let mut readers = Vec::new();
        if let Some(o) = child.stdout.take() {
            readers.push(spawn_reader(
                o,
                stdout.clone(),
                Some((StreamKind::Stdout, progress.clone())),
            ));
        }
        if let Some(e) = child.stderr.take() {
            readers.push(spawn_reader(
                e,
                stderr.clone(),
                Some((StreamKind::Stderr, progress.clone())),
            ));
        }
        let mut stdin = child.stdin.take();
        if let Some(si) = stdin.as_mut() {
            if let Some(data) = args.stdin.as_deref() {
                let _ = si.write_all(data.as_bytes()).await;
            }
            let _ = si.flush().await;
        }
        // Drop the handle to send EOF when the command reads until end-of-input.
        if args.close_stdin {
            stdin = None;
        }

        let now = Instant::now();
        let yield_at = yield_deadline(now, args.yield_time_ms, DEFAULT_RUN_YIELD_MS);
        let kill_at = args.timeout_ms.map(|ms| now + Duration::from_millis(ms));

        let mut session = ShellSession {
            child,
            stdin,
            stdout,
            stderr,
            readers,
            kill_at,
            command: args.command.clone(),
            started: now,
            progress: Some(progress.clone()),
            output_filter,
        };

        match drive(&mut session.child, yield_at, kill_at).await {
            Outcome::Exited(code) => {
                let output = drain_final(&mut session, max_tokens).await;
                terminal_result(code, output, false)
            }
            Outcome::TimedOut => {
                let output = drain_final(&mut session, max_tokens).await;
                terminal_result(None, output, true)
            }
            Outcome::Yielded => {
                let output = snapshot(&session, max_tokens);
                let progress = session_progress(&session);
                match cx.shell_sessions.lock().unwrap().insert(session) {
                    Ok(id) => {
                        let mut out = json!({
                            "exit_code": Value::Null, "stdout": output.stdout, "stderr": output.stderr,
                            "running": true, "timed_out": false, "session_id": id,
                            "next_step": format!("Call shell_poll with session_id={id} until running=false before interpreting this command as complete."),
                        });
                        if let Some(report) = output.output_filter {
                            out["output_filter"] = report;
                        }
                        if let Some(p) = progress {
                            out["progress"] = p;
                        }
                        ToolResult::Json(out)
                    }
                    Err(mut overflow) => {
                        // Group-wide kill so the overflow session's whole tree
                        // dies, then reap the direct child.
                        if let Some(pid) = overflow.child.id() {
                            signal_group(pid, libc::SIGKILL);
                        }
                        let _ = overflow.child.start_kill();
                        ToolResult::Error(format!(
                            "too many live shell sessions ({MAX_LIVE_SESSIONS}); \
                             poll or shell_kill an existing one before starting another"
                        ))
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// shell_poll
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellPollInput {
    /// Session id from a prior shell_run that returned running=true.
    session_id: String,
    /// Optional stdin to feed before draining.
    stdin: Option<String>,
    /// Close (EOF) the stdin stream after writing `stdin`.
    #[serde(default)]
    close_stdin: bool,
    /// Optional signal to send before draining: "int" (SIGINT, like Ctrl-C),
    /// "term" (SIGTERM), or "kill" (SIGKILL). Lets you ask a server/watcher to
    /// stop and then drain its shutdown output in the same call. Unlike
    /// shell_kill, the session stays alive if the process ignores the signal,
    /// so you can poll again or escalate.
    signal: Option<String>,
    /// Cooperative yield in milliseconds before returning if still running.
    /// Defaults to 5000; set 0 to block until the command exits or times out.
    yield_time_ms: Option<u64>,
    /// Output token budget for this drain (default 10000).
    max_output_tokens: Option<usize>,
    /// Optional post-capture line filter for this drain. When omitted, the
    /// filter from the originating shell_run is reused, if any.
    output_filter: Option<ShellOutputFilterInput>,
}

pub struct ShellPoll;

#[async_trait]
impl Tool for ShellPoll {
    fn name(&self) -> &str {
        "shell_poll"
    }
    fn description(&self) -> &str {
        "Resume a running shell session from shell_run: optionally feed stdin, close stdin, send signal=int|term|kill, and wait up to yield_time_ms for exit. Defaults to 5000ms; set yield_time_ms=0 to block until exit/timeout. Returns {exit_code, stdout, stderr, running, timed_out}; running=false closes the session. output_filter can override the originating post-capture line filter for this and later polls. If still running, poll again or use shell_kill. The originating timeout_ms still applies."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellPollInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellPollInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let max_tokens = args.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let output_filter_arg = args.output_filter;
        let output_filter_was_provided = output_filter_arg.is_some();
        let output_filter = match compile_output_filter(output_filter_arg) {
            Ok(filter) => filter,
            Err(e) => return ToolResult::Error(e),
        };

        // Take the session out so we never hold the lock across an await.
        let mut session = match cx
            .shell_sessions
            .lock()
            .unwrap()
            .map
            .remove(&args.session_id)
        {
            Some(s) => s,
            None => {
                return ToolResult::Error(format!(
                    "no such shell session: {} (it may have already exited)",
                    args.session_id
                ));
            }
        };
        if output_filter_was_provided {
            session.output_filter = output_filter;
        }

        if let Some(data) = &args.stdin
            && let Some(si) = session.stdin.as_mut()
        {
            let _ = si.write_all(data.as_bytes()).await;
            let _ = si.flush().await;
        }
        if args.close_stdin {
            session.stdin = None;
        }
        // Optional teardown signal: ask the process to stop, then fall through
        // to draining. If it exits within the yield window the session closes;
        // if it ignores the signal it survives for a follow-up poll/kill.
        if let Some(name) = args.signal.as_deref() {
            let (sig, _) = signal_for(Some(name));
            signal_child(&session, sig);
        }

        let yield_at = yield_deadline(Instant::now(), args.yield_time_ms, DEFAULT_POLL_YIELD_MS);
        match drive(&mut session.child, yield_at, session.kill_at).await {
            Outcome::Exited(code) => {
                let output = drain_final(&mut session, max_tokens).await;
                ToolResult::Json(terminal_json(code, output, false))
            }
            Outcome::TimedOut => {
                let output = drain_final(&mut session, max_tokens).await;
                ToolResult::Json(terminal_json(None, output, true))
            }
            Outcome::Yielded => {
                let output = snapshot(&session, max_tokens);
                let progress = session_progress(&session);
                // Re-insert directly (we already own the slot; can't overflow).
                cx.shell_sessions
                    .lock()
                    .unwrap()
                    .map
                    .insert(args.session_id.clone(), session);
                let mut out = json!({
                    "exit_code": Value::Null, "stdout": output.stdout, "stderr": output.stderr,
                    "running": true, "timed_out": false, "session_id": args.session_id,
                    "next_step": format!("Call shell_poll again with session_id={} until running=false before interpreting this command as complete.", args.session_id),
                });
                if let Some(report) = output.output_filter {
                    out["output_filter"] = report;
                }
                if let Some(p) = progress {
                    out["progress"] = p;
                }
                ToolResult::Json(out)
            }
        }
    }
}

// ---------------------------------------------------------------------------
// shell_kill
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellKillInput {
    /// Session id to terminate.
    session_id: String,
    /// Signal to send first: "term" (SIGTERM, default, graceful), "int"
    /// (SIGINT), or "kill" (SIGKILL, immediate). If the process hasn't exited
    /// within grace_ms, it is force-killed (SIGKILL).
    signal: Option<String>,
    /// Grace window in ms to wait for exit after the signal before
    /// force-killing (default 2000).
    grace_ms: Option<u64>,
    /// Output token budget for the final drain (default 10000).
    max_output_tokens: Option<usize>,
    /// Optional post-capture line filter for the final drain. When omitted, the
    /// filter from the originating shell_run is reused, if any.
    output_filter: Option<ShellOutputFilterInput>,
}

pub struct ShellKill;

#[async_trait]
impl Tool for ShellKill {
    fn name(&self) -> &str {
        "shell_kill"
    }
    fn description(&self) -> &str {
        "Terminate a running shell session. Sends signal (term|int|kill, default term), waits up to grace_ms for graceful exit, then force-kills. Drains and returns final {exit_code, stdout, stderr, running:false, killed}. output_filter can override the originating post-capture line filter for the final drain. Use this to stop a dev server or watch process you started with shell_run + yield_time_ms."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellKillInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellKillInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let max_tokens = args.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        let output_filter_arg = args.output_filter;
        let output_filter_was_provided = output_filter_arg.is_some();
        let output_filter = match compile_output_filter(output_filter_arg) {
            Ok(filter) => filter,
            Err(e) => return ToolResult::Error(e),
        };

        let mut session = match cx
            .shell_sessions
            .lock()
            .unwrap()
            .map
            .remove(&args.session_id)
        {
            Some(s) => s,
            None => {
                return ToolResult::Error(format!(
                    "no such shell session: {} (it may have already exited)",
                    args.session_id
                ));
            }
        };
        if output_filter_was_provided {
            session.output_filter = output_filter;
        }

        let (sig, sig_name) = signal_for(args.signal.as_deref());
        signal_child(&session, sig);

        // Wait for graceful exit up to grace_ms; drive escalates to SIGKILL via
        // its kill_at branch if the process ignores the first signal.
        let grace = Duration::from_millis(args.grace_ms.unwrap_or(2000));
        let kill_at = Some(Instant::now() + grace);
        let outcome = drive(&mut session.child, None, kill_at).await;
        let output = drain_final(&mut session, max_tokens).await;
        // `escalated_to_sigkill` means the requested signal was ignored and we
        // had to force-kill after grace — NOT merely "SIGKILL was requested".
        let (exit_code, escalated) = match outcome {
            Outcome::Exited(code) => (code, false),
            Outcome::TimedOut => (None, true),
            Outcome::Yielded => (None, false), // unreachable (no yield_at)
        };
        let mut out = json!({
            "exit_code": exit_code,
            "stdout": output.stdout,
            "stderr": output.stderr,
            "running": false,
            "killed": true,
            "signal_sent": sig_name,
            "escalated_to_sigkill": escalated,
        });
        if let Some(report) = output.output_filter {
            out["output_filter"] = report;
        }
        ToolResult::Json(out)
    }
}

// ---------------------------------------------------------------------------
// shell_list
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellListInput {}

pub struct ShellList;

#[async_trait]
impl Tool for ShellList {
    fn name(&self) -> &str {
        "shell_list"
    }
    fn description(&self) -> &str {
        "List the live (still-running) shell sessions for this dispatch: their session_id, the command, and how long they've been running. Use this to recover a session_id you lost, or to find orphaned long-running processes to shell_poll or shell_kill."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellListInput>()
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            ..Default::default()
        }
    }
    async fn call(&self, _input: Value, cx: &ToolCx) -> ToolResult {
        let now = Instant::now();
        let guard = cx.shell_sessions.lock().unwrap();
        let mut sessions: Vec<Value> = guard
            .map
            .iter()
            .map(|(id, s)| {
                json!({
                    "session_id": id,
                    "command": s.command,
                    "elapsed_secs": now.saturating_duration_since(s.started).as_secs(),
                })
            })
            .collect();
        sessions.sort_by(|a, b| a["session_id"].as_str().cmp(&b["session_id"].as_str()));
        ToolResult::Json(json!({ "sessions": sessions, "count": sessions.len() }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cx() -> ToolCx {
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(ShellSessions::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(crate::tool_defaults::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    fn as_json(r: ToolResult) -> Value {
        match r {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn shell_env_overlay_reaches_children_and_model_env_wins() {
        let mut cx = cx();
        cx.shell_env = Arc::new(std::collections::BTreeMap::from([(
            "ENV_UNIFY_PROBE".to_string(),
            "from-host".to_string(),
        )]));
        let v = as_json(
            ShellRun
                .call(json!({"command": "echo $ENV_UNIFY_PROBE"}), &cx)
                .await,
        );
        assert_eq!(v["exit_code"], 0);
        assert!(
            v["stdout"].as_str().unwrap_or("").contains("from-host"),
            "host shell_env must reach shell children: {v}"
        );

        // Model-supplied per-call env takes precedence over the host overlay.
        let v = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "echo $ENV_UNIFY_PROBE",
                        "env": {"ENV_UNIFY_PROBE": "from-model"}
                    }),
                    &cx,
                )
                .await,
        );
        assert!(
            v["stdout"].as_str().unwrap_or("").contains("from-model"),
            "per-call env must win over the host overlay: {v}"
        );
    }

    #[tokio::test]
    async fn blocking_command_completes() {
        let v = as_json(ShellRun.call(json!({"command": "echo hi"}), &cx()).await);
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["running"], false);
        assert_eq!(v["timed_out"], false, "timed_out always present");
        assert!(v["session_id"].is_null(), "no session for a finished cmd");
        assert_eq!(v["stdout"], "hi\n");
    }

    #[tokio::test]
    async fn spawn_scrub_hides_host_vars_from_shell_children() {
        // SAFETY: unique key, not touched by any other test.
        unsafe {
            std::env::set_var("BRO_TEST_SCRUB_K9", "leaked-value");
        }

        // Under a scrub scope (the daemon's in-process session), the child must
        // NOT inherit the scrubbed var.
        let scrubbed = with_spawn_scrub(vec!["BRO_TEST_SCRUB_K9".to_string()], async {
            as_json(
                ShellRun
                    .call(
                        json!({"command": "printf '%s' \"$BRO_TEST_SCRUB_K9\""}),
                        &cx(),
                    )
                    .await,
            )
        })
        .await;
        assert_eq!(
            scrubbed["stdout"], "",
            "scrubbed host var must not reach the child"
        );

        // Outside any scrub scope (standalone binary), the child inherits it.
        let leaked = as_json(
            ShellRun
                .call(
                    json!({"command": "printf '%s' \"$BRO_TEST_SCRUB_K9\""}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(leaked["stdout"], "leaked-value");

        // SAFETY: cleanup of this test's unique key.
        unsafe {
            std::env::remove_var("BRO_TEST_SCRUB_K9");
        }
    }

    #[tokio::test]
    async fn shell_children_get_uncolored_output_env() {
        // apply_child_env sets NO_COLOR regardless of scrub scope.
        let v = as_json(
            ShellRun
                .call(json!({"command": "printf '%s' \"$NO_COLOR\""}), &cx())
                .await,
        );
        assert_eq!(v["stdout"], "1");
    }

    #[tokio::test]
    async fn default_yield_returns_session_for_long_command() {
        let c = cx();
        let v = as_json(ShellRun.call(json!({"command": "sleep 2"}), &c).await);
        assert_eq!(v["running"], true, "default yield should return early: {v}");
        assert!(v["exit_code"].is_null(), "running command has no exit: {v}");
        assert!(
            v["next_step"]
                .as_str()
                .is_some_and(|s| s.contains("shell_poll")),
            "missing poll guidance: {v}"
        );

        let sid = v["session_id"].as_str().unwrap().to_string();
        let _ = ShellKill
            .call(json!({"session_id": sid, "signal": "term"}), &c)
            .await;
    }

    #[tokio::test]
    async fn yielded_session_reports_running_progress() {
        // A long command that emits output up front then goes quiet should yield
        // with a `progress` block reflecting the bytes already read — the
        // session-mode counterpart to promise progress (gap note-330f1485).
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "echo hi; sleep 2"}), &c)
                .await,
        );
        assert_eq!(v["running"], true, "should yield: {v}");
        let progress = &v["progress"];
        assert!(
            progress.is_object(),
            "yielded response carries progress: {v}"
        );
        assert!(
            progress["stdout_bytes"].as_u64().unwrap_or(0) >= 3,
            "early `hi\\n` output should be counted: {v}"
        );
        assert!(
            progress["elapsed_ms"].as_u64().is_some(),
            "progress reports elapsed: {v}"
        );

        let sid = v["session_id"].as_str().unwrap().to_string();
        let _ = ShellKill
            .call(json!({"session_id": sid, "signal": "term"}), &c)
            .await;
    }

    #[tokio::test]
    async fn yield_time_zero_blocks_until_completion() {
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 0.1; echo done", "yield_time_ms": 0}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(v["running"], false, "{v}");
        assert_eq!(v["exit_code"], 0, "{v}");
        assert_eq!(v["stdout"], "done\n");
    }

    #[tokio::test]
    async fn generous_yield_blocks_slow_command_to_completion() {
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 2; echo done", "yield_time_ms": 5000}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(
            v["running"], false,
            "generous yield should finish inline: {v}"
        );
        assert_eq!(v["exit_code"], 0, "{v}");
        assert_eq!(v["stdout"], "done\n");
        assert!(
            v["session_id"].is_null(),
            "finished command should not retain a session: {v}"
        );
    }

    #[tokio::test]
    async fn short_yield_elapses_before_exit() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 2", "yield_time_ms": 50}), &c)
                .await,
        );
        assert_eq!(
            v["running"], true,
            "short yield should return a session: {v}"
        );
        assert!(
            v["session_id"].as_str().is_some(),
            "yielded command needs a session id: {v}"
        );

        let sid = v["session_id"].as_str().unwrap().to_string();
        let _ = ShellKill
            .call(json!({"session_id": sid, "signal": "term"}), &c)
            .await;
    }

    #[test]
    fn shell_path_env_prepends_user_local_bins() {
        let tmp = tempfile::tempdir().unwrap();
        let path = augmented_path_env(
            Some(OsString::from("/opt/tools")),
            Some(tmp.path().to_path_buf()),
            Some(OsString::from("/usr/bin")),
        )
        .unwrap();
        let entries: Vec<_> = std::env::split_paths(&path).collect();

        assert_eq!(entries[0], PathBuf::from("/opt/tools"));
        assert_eq!(entries[1], tmp.path().join(".local").join("bin"));
        assert_eq!(entries[2], tmp.path().join(".cargo").join("bin"));
        assert_eq!(entries[3], PathBuf::from("/usr/bin"));
    }

    #[tokio::test]
    async fn timeout_kills_runaway() {
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 5", "timeout_ms": 200}), &cx())
                .await,
        );
        assert_eq!(v["running"], false);
        assert_eq!(v["timed_out"], true);
        assert!(v["exit_code"].is_null(), "killed → no exit code");
    }

    #[tokio::test]
    async fn timeout_kills_backgrounded_grandchildren() {
        // A timed-out command must take down its WHOLE process group: a
        // backgrounded grandchild (the sccache/rustc-holding-the-build-lock
        // analog) must not survive the kill. bash echoes its own pid, which is
        // the group id because the child is spawned with process_group(0).
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "echo pgid=$$; sleep 30 & sleep 30",
                           "yield_time_ms": 0, "timeout_ms": 300}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(v["running"], false, "{v}");
        assert_eq!(v["timed_out"], true, "{v}");
        let pgid: i32 = v["stdout"]
            .as_str()
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("pgid="))
            .expect("pgid line in stdout")
            .trim()
            .parse()
            .unwrap();
        // SIGKILL is immediate, but the reparented grandchild may linger as a
        // zombie until its reaper collects it; poll with a bound instead of
        // asserting instantly. The assertion is about LIVE members (a zombie
        // holds no locks and runs no code): kill(-pgid, 0) alone is not
        // enough, because it also succeeds for zombie-only groups, and in a
        // container whose PID 1 never reaps orphans (the CI pod) that state
        // can persist past any bound.
        let mut group_gone = false;
        for _ in 0..100 {
            if !group_has_live_members(pgid) {
                group_gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            group_gone,
            "process group {pgid} still has live members after timeout kill"
        );
    }

    /// True while process group `pgid` contains at least one non-zombie
    /// member. On Linux this walks /proc (state Z members are dead for the
    /// purposes of the kill contract); elsewhere it falls back to the
    /// kill(2) signal-0 probe, which suffices where init reaps orphans
    /// promptly (macOS launchd).
    fn group_has_live_members(pgid: i32) -> bool {
        #[cfg(target_os = "linux")]
        {
            let Ok(entries) = std::fs::read_dir("/proc") else {
                // SAFETY: kill(2) with signal 0 probes group existence only.
                return (unsafe { libc::kill(-pgid, 0) }) == 0;
            };
            for entry in entries.flatten() {
                if !entry.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()) {
                    continue;
                }
                let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                    continue;
                };
                // /proc/<pid>/stat: "pid (comm) state ppid pgrp ..."; comm may
                // contain spaces/parens, so parse from the LAST ')'.
                let Some(rest) = stat.rfind(')').map(|i| &stat[i + 1..]) else {
                    continue;
                };
                let mut fields = rest.split_whitespace();
                let state = fields.next();
                let pgrp = fields.nth(1).and_then(|s| s.parse::<i32>().ok());
                if pgrp == Some(pgid) && state != Some("Z") {
                    return true;
                }
            }
            false
        }
        #[cfg(not(target_os = "linux"))]
        {
            // SAFETY: kill(2) with signal 0 probes group existence only.
            (unsafe { libc::kill(-pgid, 0) }) == 0
        }
    }

    #[tokio::test]
    async fn yields_then_poll_completes() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 1; echo done", "yield_time_ms": 50}),
                    &c,
                )
                .await,
        );
        assert_eq!(v["running"], true, "slow cmd should yield: {v}");
        assert!(
            v["next_step"]
                .as_str()
                .is_some_and(|s| s.contains("shell_poll")),
            "missing poll guidance: {v}"
        );
        let sid = v["session_id"].as_str().unwrap().to_string();

        let p = as_json(
            ShellPoll
                .call(json!({"session_id": sid, "yield_time_ms": 3000}), &c)
                .await,
        );
        assert_eq!(p["running"], false, "should have finished: {p}");
        assert_eq!(p["exit_code"], 0);
        assert!(
            p["stdout"].as_str().unwrap().contains("done"),
            "final output: {p}"
        );
    }

    #[tokio::test]
    async fn shell_poll_short_yield_keeps_session_running() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 2", "yield_time_ms": 50}), &c)
                .await,
        );
        let sid = v["session_id"].as_str().unwrap().to_string();

        let p = as_json(
            ShellPoll
                .call(json!({"session_id": sid, "yield_time_ms": 50}), &c)
                .await,
        );
        assert_eq!(
            p["running"], true,
            "short poll yield should keep session: {p}"
        );

        let sid = p["session_id"].as_str().unwrap().to_string();
        let _ = ShellKill
            .call(json!({"session_id": sid, "signal": "term"}), &c)
            .await;
    }

    #[tokio::test]
    async fn shell_poll_yield_zero_blocks_until_completion() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 0.2; echo done", "yield_time_ms": 50}),
                    &c,
                )
                .await,
        );
        let sid = v["session_id"].as_str().unwrap().to_string();

        let p = as_json(
            ShellPoll
                .call(json!({"session_id": sid, "yield_time_ms": 0}), &c)
                .await,
        );
        assert_eq!(
            p["running"], false,
            "zero poll yield should block to exit: {p}"
        );
        assert_eq!(p["exit_code"], 0, "{p}");
        assert_eq!(p["stdout"], "done\n");
    }

    #[tokio::test]
    async fn stdin_is_fed() {
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "read x; echo got=$x", "stdin": "hello\n"}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(v["exit_code"], 0, "{v}");
        assert!(v["stdout"].as_str().unwrap().contains("got=hello"), "{v}");
    }

    #[tokio::test]
    async fn close_stdin_lets_read_until_eof_finish() {
        // `cat` reads until EOF; without close_stdin it would hang past the
        // yield and become a session. With close_stdin it completes inline.
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "cat", "stdin": "abc\n", "close_stdin": true,
                           "yield_time_ms": 2000}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(v["running"], false, "EOF should let cat exit: {v}");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["stdout"], "abc\n");
    }

    #[tokio::test]
    async fn max_output_tokens_caps_tail() {
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "for i in $(seq 1 1000); do echo line$i; done",
                           "max_output_tokens": 5}),
                    &cx(),
                )
                .await,
        );
        let out = v["stdout"].as_str().unwrap();
        assert!(out.contains("earlier bytes truncated"), "marker: {out}");
        assert!(out.contains("line1000"), "tail kept: {out}");
        assert!(!out.contains("line1\n"), "head dropped: {out}");
    }

    #[tokio::test]
    async fn output_filter_keeps_matching_lines_without_changing_exit_code() {
        let v = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "printf 'noise\\nBUILD SUCCESSFUL\\nerror: real\\n'; printf 'warning: keep\\nignore\\n' >&2; exit 7",
                        "yield_time_ms": 0,
                        "output_filter": {
                            "stdout": ["BUILD", "error:"],
                            "stderr": ["warning:"]
                        }
                    }),
                    &cx(),
                )
                .await,
        );

        assert_eq!(
            v["exit_code"], 7,
            "filter must not wrap command status: {v}"
        );
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.contains("BUILD SUCCESSFUL"), "{v}");
        assert!(stdout.contains("error: real"), "{v}");
        assert!(!stdout.contains("noise"), "{v}");
        let stderr = v["stderr"].as_str().unwrap();
        assert!(stderr.contains("warning: keep"), "{v}");
        assert!(!stderr.contains("ignore"), "{v}");
        assert_eq!(v["output_filter"]["stdout"]["kept_lines"], 2, "{v}");
        assert_eq!(v["output_filter"]["stdout"]["dropped_lines"], 1, "{v}");
        assert_eq!(v["output_filter"]["stderr"]["kept_lines"], 1, "{v}");
        assert_eq!(v["output_filter"]["stderr"]["dropped_lines"], 1, "{v}");
    }

    #[tokio::test]
    async fn output_filter_rejects_invalid_regex() {
        let r = ShellRun
            .call(
                json!({
                    "command": "echo hi",
                    "output_filter": {"stdout": ["("]}
                }),
                &cx(),
            )
            .await;

        assert!(
            matches!(r, ToolResult::Error(ref e) if e.contains("invalid regex")),
            "{r:?}"
        );
    }

    #[tokio::test]
    async fn output_filter_accepts_string_pattern_shorthand() {
        let v = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "printf 'noise\nBUILD SUCCESSFUL\nerror: real\n'; printf 'warning: keep\nignore\n' >&2; exit 7",
                        "yield_time_ms": 0,
                        "output_filter": {
                            "stdout": "BUILD SUCCESSFUL|error:",
                            "stderr": "warning:"
                        }
                    }),
                    &cx(),
                )
                .await,
        );

        assert_eq!(v["exit_code"], 7, "{v}");
        let stdout = v["stdout"].as_str().unwrap();
        assert!(stdout.contains("BUILD SUCCESSFUL"), "{v}");
        assert!(stdout.contains("error: real"), "{v}");
        assert!(!stdout.contains("noise"), "{v}");
        let stderr = v["stderr"].as_str().unwrap();
        assert!(stderr.contains("warning: keep"), "{v}");
        assert!(!stderr.contains("ignore"), "{v}");
    }

    #[tokio::test]
    async fn yielded_session_reuses_output_filter_on_poll() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "echo keep-start; echo noise-start; sleep 1; echo keep-end; echo noise-end",
                        "yield_time_ms": 100,
                        "output_filter": {"stdout": ["keep"]}
                    }),
                    &c,
                )
                .await,
        );
        assert_eq!(v["running"], true, "{v}");
        let first = v["stdout"].as_str().unwrap();
        assert!(first.contains("keep-start"), "{v}");
        assert!(!first.contains("noise-start"), "{v}");

        let sid = v["session_id"].as_str().unwrap().to_string();
        let p = as_json(
            ShellPoll
                .call(json!({"session_id": sid, "yield_time_ms": 0}), &c)
                .await,
        );
        assert_eq!(p["running"], false, "{p}");
        let final_out = p["stdout"].as_str().unwrap();
        assert!(final_out.contains("keep-end"), "{p}");
        assert!(!final_out.contains("noise-end"), "{p}");
        assert!(p["output_filter"]["stdout"].is_object(), "{p}");
    }

    #[tokio::test]
    async fn poll_can_clear_originating_output_filter() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "echo keep-start; echo noise-start; sleep 1; echo keep-end; echo noise-end",
                        "yield_time_ms": 100,
                        "output_filter": {"stdout": ["keep"]}
                    }),
                    &c,
                )
                .await,
        );
        assert_eq!(v["running"], true, "{v}");
        assert!(
            !v["stdout"].as_str().unwrap().contains("noise-start"),
            "{v}"
        );

        let sid = v["session_id"].as_str().unwrap().to_string();
        let p = as_json(
            ShellPoll
                .call(
                    json!({"session_id": sid, "yield_time_ms": 0, "output_filter": {}}),
                    &c,
                )
                .await,
        );
        assert_eq!(p["running"], false, "{p}");
        let final_out = p["stdout"].as_str().unwrap();
        assert!(final_out.contains("keep-end"), "{p}");
        assert!(final_out.contains("noise-end"), "{p}");
        assert!(p.get("output_filter").is_none(), "{p}");
    }

    #[tokio::test]
    async fn backgrounded_pipe_holder_does_not_hang() {
        // The direct child exits immediately but leaves a process holding the
        // stdout pipe open. drain_final must NOT block on the reader; the
        // bounded grace + abort keeps this fast. Guard with an outer timeout so
        // a regression fails loudly instead of hanging the suite.
        let c = cx();
        let fut = ShellRun.call(
            json!({"command": "sleep 30 & echo started", "yield_time_ms": 4000}),
            &c,
        );
        // The command finishes (echo + bash exits) well under the yield, but the
        // backgrounded sleep holds the pipe. Bound to READER_DRAIN_GRACE + slack.
        let v = as_json(
            tokio::time::timeout(Duration::from_secs(6), fut)
                .await
                .expect("drain_final hung on a backgrounded pipe holder"),
        );
        assert_eq!(v["running"], false, "bash itself exited: {v}");
        assert!(v["stdout"].as_str().unwrap().contains("started"), "{v}");
    }

    #[tokio::test]
    async fn shell_kill_terminates_session() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 60", "yield_time_ms": 50}), &c)
                .await,
        );
        assert_eq!(v["running"], true, "{v}");
        let sid = v["session_id"].as_str().unwrap().to_string();

        let k = as_json(
            ShellKill
                .call(json!({"session_id": sid, "signal": "term"}), &c)
                .await,
        );
        assert_eq!(k["killed"], true, "{k}");
        assert_eq!(k["running"], false, "{k}");
        assert_eq!(k["signal_sent"], "term", "{k}");
        assert_eq!(
            k["escalated_to_sigkill"], false,
            "sleep dies on SIGTERM: {k}"
        );
        // Session is gone afterward.
        assert!(c.shell_sessions.lock().unwrap().map.is_empty());
    }

    #[tokio::test]
    async fn shell_poll_signal_stops_and_drains_in_one_call() {
        // A process that traps SIGTERM-ish but dies on SIGINT; simplest: plain
        // `sleep` dies on SIGINT. Start it, then poll with signal=int and a
        // window long enough to observe the exit — one call stops + drains.
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 60", "yield_time_ms": 50}), &c)
                .await,
        );
        let sid = v["session_id"].as_str().unwrap().to_string();
        let p = as_json(
            ShellPoll
                .call(
                    json!({"session_id": sid, "signal": "int", "yield_time_ms": 3000}),
                    &c,
                )
                .await,
        );
        assert_eq!(p["running"], false, "SIGINT should stop sleep: {p}");
        // Session closed after it exited.
        assert!(c.shell_sessions.lock().unwrap().map.is_empty());
    }

    #[tokio::test]
    async fn shell_list_reports_and_recovers_sessions() {
        let c = cx();
        // No live sessions initially.
        let empty = as_json(ShellList.call(json!({}), &c).await);
        assert_eq!(empty["count"], 0, "{empty}");

        // Start one that yields.
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 30", "yield_time_ms": 50}), &c)
                .await,
        );
        let sid = v["session_id"].as_str().unwrap().to_string();

        let listed = as_json(ShellList.call(json!({}), &c).await);
        assert_eq!(listed["count"], 1, "{listed}");
        assert_eq!(listed["sessions"][0]["session_id"], sid, "{listed}");
        assert_eq!(listed["sessions"][0]["command"], "sleep 30", "{listed}");

        // Recover via the listed id and kill it.
        let _ = ShellKill.call(json!({"session_id": sid}), &c).await;
        let after = as_json(ShellList.call(json!({}), &c).await);
        assert_eq!(after["count"], 0, "killed session gone: {after}");
    }

    #[tokio::test]
    async fn env_vars_are_injected() {
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "echo port=$PORT", "env": {"PORT": "3000"}}),
                    &cx(),
                )
                .await,
        );
        assert_eq!(v["exit_code"], 0, "{v}");
        assert!(v["stdout"].as_str().unwrap().contains("port=3000"), "{v}");
    }

    #[tokio::test]
    async fn unknown_session_errors() {
        let r = ShellPoll.call(json!({"session_id": "sh-999"}), &cx()).await;
        assert!(r.is_error());
        let r = ShellKill.call(json!({"session_id": "sh-999"}), &cx()).await;
        assert!(r.is_error());
    }
}

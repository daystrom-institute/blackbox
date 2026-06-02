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
//! `kill_on_drop`, so abandoned sessions die when the `ToolCx` drops.

use crate::promise::{PromiseProgress, StreamKind};
use crate::tool::{Tool, ToolAnnotations, ToolCx, ToolResult, schema_for};
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Per-stream in-memory buffer cap. Beyond this, output is counted-and-dropped
/// rather than retained, so a runaway producer can't exhaust memory.
const MAX_BUF_BYTES: usize = 8 * 1024 * 1024;
/// Default returned-output budget (~40 KB at a 4-bytes/token heuristic).
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
/// Default cooperative yield. Long commands should not make the whole agent
/// turn look hung just because the model forgot to set `yield_time_ms`.
const DEFAULT_YIELD_MS: u64 = 1_000;
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
    /// Shared progress for promise mode — the readers heartbeat into this so
    /// `promise_status` can expose running-progress metadata.
    progress: Option<Arc<PromiseProgress>>,
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
/// still making progress. Mirrors the `progress` field on promise snapshots.
fn session_progress(session: &ShellSession) -> Option<Value> {
    let progress = session.progress.as_ref()?;
    let started_ms =
        crate::promise::now_ms().saturating_sub(session.started.elapsed().as_millis() as u64);
    Some(progress.snapshot(started_ms))
}

enum PromiseOutcome {
    Exited(Option<i32>),
    TimedOut,
    Cancelled,
    Failed(String),
}

/// Drive a child until it exits, the yield deadline elapses, or the hard-kill
/// deadline elapses (in which case it is killed). Holds no lock.
async fn drive(child: &mut Child, yield_at: Option<Instant>, kill_at: Option<Instant>) -> Outcome {
    let far = Instant::now() + Duration::from_secs(31_536_000);
    let y = yield_at.unwrap_or(far);
    let k = kill_at.unwrap_or(far);
    tokio::select! {
        s = child.wait() => Outcome::Exited(s.ok().and_then(|st| st.code())),
        _ = sleep_until(y), if yield_at.is_some() => Outcome::Yielded,
        _ = sleep_until(k), if kill_at.is_some() => {
            let _ = child.kill().await;
            Outcome::TimedOut
        }
    }
}

async fn drive_promise(
    child: &mut Child,
    kill_at: Option<Instant>,
    cancel: &mut watch::Receiver<bool>,
) -> PromiseOutcome {
    enum Event {
        Exited(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    let event = if let Some(kill_at) = kill_at {
        tokio::select! {
            s = child.wait() => Event::Exited(s),
            _ = sleep_until(kill_at) => Event::TimedOut,
            _ = cancel.changed() => Event::Cancelled,
        }
    } else {
        tokio::select! {
            s = child.wait() => Event::Exited(s),
            _ = cancel.changed() => Event::Cancelled,
        }
    };

    match event {
        Event::Exited(Ok(status)) => PromiseOutcome::Exited(status.code()),
        Event::Exited(Err(e)) => PromiseOutcome::Failed(format!("wait failed: {e}")),
        Event::TimedOut => {
            let _ = child.kill().await;
            PromiseOutcome::TimedOut
        }
        Event::Cancelled => {
            let _ = child.kill().await;
            PromiseOutcome::Cancelled
        }
    }
}

async fn run_shell_promise(
    mut session: ShellSession,
    cx: ToolCx,
    promise_id: String,
    stdout_to: Option<String>,
    max_tokens: usize,
    mut cancel_rx: watch::Receiver<bool>,
) {
    let outcome = drive_promise(&mut session.child, session.kill_at, &mut cancel_rx).await;
    let (so, se) = drain_final(&mut session, max_tokens).await;
    match outcome {
        PromiseOutcome::Exited(code) => {
            let result = terminal_result(&cx, stdout_to.as_deref(), code, so, se, false);
            settle_from_tool_result(&cx, &promise_id, result);
        }
        PromiseOutcome::TimedOut => {
            let result = terminal_result(&cx, stdout_to.as_deref(), None, so, se, true);
            settle_from_tool_result(&cx, &promise_id, result);
        }
        PromiseOutcome::Cancelled => {
            let result = json!({
                "exit_code": Value::Null,
                "stdout": so,
                "stderr": se,
                "running": false,
                "timed_out": false,
                "cancelled": true,
            });
            cx.promises
                .lock()
                .unwrap()
                .settle_cancelled(&promise_id, result);
        }
        PromiseOutcome::Failed(err) => cx.promises.lock().unwrap().settle_failed(&promise_id, err),
    }
}

fn settle_from_tool_result(cx: &ToolCx, promise_id: &str, result: ToolResult) {
    match result {
        ToolResult::Json(v) => cx.promises.lock().unwrap().settle_completed(promise_id, v),
        ToolResult::Text(t) => cx
            .promises
            .lock()
            .unwrap()
            .settle_completed(promise_id, json!({ "text": t })),
        ToolResult::Error(e) => cx.promises.lock().unwrap().settle_failed(promise_id, e),
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
fn snapshot(session: &ShellSession, max_tokens: usize) -> (String, String) {
    let (so, so_drop) = session.stdout.lock().unwrap().take();
    let (se, se_drop) = session.stderr.lock().unwrap().take();
    (
        render(so, so_drop, max_tokens),
        render(se, se_drop, max_tokens),
    )
}

/// After a child has exited (or been killed) give its readers a bounded grace
/// window to flush remaining bytes, then ABORT stragglers and drain.
///
/// The bound is load-bearing: when a command backgrounds a process that
/// inherited the stdout/stderr pipe (`cmd &`), the direct child exits but the
/// pipe stays open, so a reader awaiting EOF would block forever. We abort it
/// instead of hanging the agent loop.
async fn drain_final(session: &mut ShellSession, max_tokens: usize) -> (String, String) {
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

/// Send `sig` to a session's child (no-op if already reaped).
fn signal_child(session: &ShellSession, sig: i32) {
    if let Some(pid) = session.child.id() {
        // SAFETY: kill(2) with a real pid and a constant signal; ESRCH on an
        // already-dead pid is harmless.
        unsafe {
            libc::kill(pid as i32, sig);
        }
    }
}

/// Build the terminal JSON for an exited/timed-out/killed session.
fn terminal_json(exit_code: Option<i32>, stdout: String, stderr: String, timed_out: bool) -> Value {
    json!({
        "exit_code": exit_code,
        "stdout": stdout,
        "stderr": stderr,
        "running": false,
        "timed_out": timed_out,
    })
}

/// Build a terminal `shell_run` result, routing stdout into a clipboard
/// register when `stdout_to` is set (the ref ABI producer path) instead of
/// returning it inline. stderr always stays inline so failures are visible.
fn terminal_result(
    cx: &ToolCx,
    stdout_to: Option<&str>,
    exit_code: Option<i32>,
    stdout: String,
    stderr: String,
    timed_out: bool,
) -> ToolResult {
    let Some(register) = stdout_to else {
        return ToolResult::Json(terminal_json(exit_code, stdout, stderr, timed_out));
    };
    let byte_len = stdout.len();
    let mut clip = cx.clipboard.lock().unwrap();
    let evicted = match clip.put_tool_result(register, stdout) {
        Ok(e) => e,
        Err(e) => return ToolResult::Error(e.to_string()),
    };
    let sha = clip
        .meta_of(register)
        .and_then(|m| m.get("slice_sha256").cloned());
    let mut out = json!({
        "exit_code": exit_code,
        "stdout_register": register,
        "stdout_bytes": byte_len,
        "stdout_sha256": sha,
        "stderr": stderr,
        "running": false,
        "timed_out": timed_out,
    });
    crate::clipboard::annotate_evicted(&mut out, evicted);
    ToolResult::Json(out)
}

fn shell_path_env() -> Option<OsString> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    augmented_path_env(
        std::env::var_os("BRO_EXTRA_PATH"),
        home,
        std::env::var_os("PATH"),
    )
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
    /// Execution mode. Omit or set "inline" for normal shell_run behavior; set
    /// "promise" to start immediately and return a promise_id for promise_*.
    #[serde(default)]
    mode: Option<String>,
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
    /// Ref ABI (chaining): capture stdout into this clipboard register instead
    /// of returning it, so large output never enters your context. The response
    /// reports the register + byte count + sha; consume it with clip_paste /
    /// file_write{from} / clip_peek. Only honored when the command finishes
    /// within this call (not for yielded/background sessions). stderr is still
    /// returned inline. See design/bro-harness/bro-harness-tool-chaining.md.
    #[serde(default)]
    stdout_to: Option<String>,
    /// Ref ABI (chaining): feed this clipboard register's content as stdin
    /// (after any literal `stdin`), without it passing through your context.
    #[serde(default)]
    stdin_from: Option<String>,
}

pub struct ShellRun;

#[async_trait]
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }
    fn description(&self) -> &str {
        "Run a shell command in the worktree (bash -lc). Returns {exit_code, stdout, stderr, running, timed_out}. exit_code is null while running; it is ALSO null once terminated by a signal (UNIX has no exit code for signal-killed processes) — so exit_code:null with running:false means signal-terminated, not an error. Long commands cooperatively yield by default after ~1s with running=true + session_id; you MUST continue with shell_poll until running=false before trusting completion. Set yield_time_ms to tune the first wait; set yield_time_ms=0 only when deliberately blocking until completion/timeout. Set mode=\"promise\" to start immediately and return a promise_id; then use promise_status/wait/when_all/when_any/cancel. Promise completion automatically injects a hidden HARNESS_EVENT turn at a safe boundary unless a terminal wait already returned the result. timeout_ms hard-kills a runaway; max_output_tokens caps output (tail kept). stdin feeds initial input; set close_stdin to send EOF (needed for read-until-EOF commands); env injects environment variables. Categorically destructive commands (rm -rf /, git reset --hard, kill-by-port, etc.) are refused."
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
        let cwd =
            match crate::workspace::resolve_in_root(&cx.root, args.cwd.as_deref().unwrap_or(".")) {
                Ok(p) => p,
                Err(e) => return ToolResult::Error(e.to_string()),
            };
        let promise_mode = match args.mode.as_deref().unwrap_or("inline") {
            "inline" => false,
            "promise" => true,
            other => return ToolResult::Error(format!("unsupported shell_run mode: {other}")),
        };
        let max_tokens = args.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);

        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &args.command])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if let Some(path) = shell_path_env() {
            cmd.env("PATH", path);
        }
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
        // Ref ABI: resolve a stdin-feeding register before touching the child.
        let stdin_from_data = match &args.stdin_from {
            Some(reg) => match cx.clipboard.lock().unwrap().consume_text(reg) {
                Ok(t) => Some(t),
                Err(e) => return ToolResult::Error(e),
            },
            None => None,
        };
        let mut stdin = child.stdin.take();
        if let Some(si) = stdin.as_mut() {
            for data in [args.stdin.as_deref(), stdin_from_data.as_deref()]
                .into_iter()
                .flatten()
            {
                let _ = si.write_all(data.as_bytes()).await;
            }
            let _ = si.flush().await;
        }
        // Drop the handle to send EOF when the command reads until end-of-input.
        if args.close_stdin {
            stdin = None;
        }

        let now = Instant::now();
        let yield_ms = args.yield_time_ms.unwrap_or(DEFAULT_YIELD_MS);
        let yield_at = (yield_ms > 0).then(|| now + Duration::from_millis(yield_ms));
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
        };

        if promise_mode {
            let detail = json!({
                "command": args.command,
                "cwd": cwd.to_string_lossy(),
                "timeout_ms": args.timeout_ms,
            });
            let (promise_id, cancel_rx) = cx.promises.lock().unwrap().start("shell_run", detail, Some(progress));
            let cx_bg = cx.clone();
            let promise_id_bg = promise_id.clone();
            let stdout_to = args.stdout_to.clone();
            tokio::spawn(async move {
                run_shell_promise(
                    session,
                    cx_bg,
                    promise_id_bg,
                    stdout_to,
                    max_tokens,
                    cancel_rx,
                )
                .await;
            });
            return ToolResult::Json(json!({
                "promise_id": promise_id,
                "state": "running",
                "running": true,
                "next_step": format!("Use promise_status or promise_wait with promise_id={promise_id}; completion automatically injects a hidden HARNESS_EVENT turn when it settles."),
            }));
        }

        match drive(&mut session.child, yield_at, kill_at).await {
            Outcome::Exited(code) => {
                let (so, se) = drain_final(&mut session, max_tokens).await;
                terminal_result(cx, args.stdout_to.as_deref(), code, so, se, false)
            }
            Outcome::TimedOut => {
                let (so, se) = drain_final(&mut session, max_tokens).await;
                terminal_result(cx, args.stdout_to.as_deref(), None, so, se, true)
            }
            Outcome::Yielded => {
                let (so, se) = snapshot(&session, max_tokens);
                let progress = session_progress(&session);
                match cx.shell_sessions.lock().unwrap().insert(session) {
                    Ok(id) => {
                        let mut out = json!({
                            "exit_code": Value::Null, "stdout": so, "stderr": se,
                            "running": true, "timed_out": false, "session_id": id,
                            "next_step": format!("Call shell_poll with session_id={id} until running=false before interpreting this command as complete."),
                        });
                        if let Some(p) = progress {
                            out["progress"] = p;
                        }
                        ToolResult::Json(out)
                    }
                    Err(mut overflow) => {
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
    /// Cooperative yield in ms before returning if still running (default 5000).
    yield_time_ms: Option<u64>,
    /// Output token budget for this drain (default 10000).
    max_output_tokens: Option<usize>,
}

pub struct ShellPoll;

#[async_trait]
impl Tool for ShellPoll {
    fn name(&self) -> &str {
        "shell_poll"
    }
    fn description(&self) -> &str {
        "Resume a running shell session from shell_run: drain new stdout/stderr, optionally feed stdin (set close_stdin to send EOF) or send a teardown signal (signal=int|term|kill, e.g. Ctrl-C a dev server then drain its shutdown output in one call), and wait up to yield_time_ms for it to finish. Returns {exit_code, stdout, stderr, running, timed_out}; exit_code is null while running. When running=false the session is closed; if the process ignores the signal the session stays alive for a follow-up poll. The originating timeout_ms still applies."
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

        let yield_at =
            Some(Instant::now() + Duration::from_millis(args.yield_time_ms.unwrap_or(5000)));
        match drive(&mut session.child, yield_at, session.kill_at).await {
            Outcome::Exited(code) => {
                let (so, se) = drain_final(&mut session, max_tokens).await;
                ToolResult::Json(terminal_json(code, so, se, false))
            }
            Outcome::TimedOut => {
                let (so, se) = drain_final(&mut session, max_tokens).await;
                ToolResult::Json(terminal_json(None, so, se, true))
            }
            Outcome::Yielded => {
                let (so, se) = snapshot(&session, max_tokens);
                let progress = session_progress(&session);
                // Re-insert directly (we already own the slot; can't overflow).
                cx.shell_sessions
                    .lock()
                    .unwrap()
                    .map
                    .insert(args.session_id.clone(), session);
                let mut out = json!({
                    "exit_code": Value::Null, "stdout": so, "stderr": se,
                    "running": true, "timed_out": false, "session_id": args.session_id,
                    "next_step": format!("Call shell_poll again with session_id={} until running=false before interpreting this command as complete.", args.session_id),
                });
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
}

pub struct ShellKill;

#[async_trait]
impl Tool for ShellKill {
    fn name(&self) -> &str {
        "shell_kill"
    }
    fn description(&self) -> &str {
        "Terminate a running shell session. Sends signal (term|int|kill, default term), waits up to grace_ms for graceful exit, then force-kills. Drains and returns final {exit_code, stdout, stderr, running:false, killed}. Use this to stop a dev server or watch process you started with shell_run + yield_time_ms."
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

        let (sig, sig_name) = signal_for(args.signal.as_deref());
        signal_child(&session, sig);

        // Wait for graceful exit up to grace_ms; drive escalates to SIGKILL via
        // its kill_at branch if the process ignores the first signal.
        let grace = Duration::from_millis(args.grace_ms.unwrap_or(2000));
        let kill_at = Some(Instant::now() + grace);
        let outcome = drive(&mut session.child, None, kill_at).await;
        let (so, se) = drain_final(&mut session, max_tokens).await;
        // `escalated_to_sigkill` means the requested signal was ignored and we
        // had to force-kill after grace — NOT merely "SIGKILL was requested".
        let (exit_code, escalated) = match outcome {
            Outcome::Exited(code) => (code, false),
            Outcome::TimedOut => (None, true),
            Outcome::Yielded => (None, false), // unreachable (no yield_at)
        };
        ToolResult::Json(json!({
            "exit_code": exit_code,
            "stdout": so,
            "stderr": se,
            "running": false,
            "killed": true,
            "signal_sent": sig_name,
            "escalated_to_sigkill": escalated,
        }))
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
            promises: Arc::new(Mutex::new(crate::promise::PromiseStore::default())),
            clipboard: Arc::new(Mutex::new(crate::clipboard::Registers::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
        }
    }

    fn as_json(r: ToolResult) -> Value {
        match r {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        }
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
        assert!(progress.is_object(), "yielded response carries progress: {v}");
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
    async fn promise_mode_returns_running_promise_then_waits() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 0.1; echo promised", "mode": "promise"}),
                    &c,
                )
                .await,
        );
        assert_eq!(v["running"], true, "{v}");
        let pid = v["promise_id"].as_str().unwrap().to_string();

        let waited = as_json(
            crate::promise::PromiseWait
                .call(json!({"promise_id": pid, "timeout_ms": 3000}), &c)
                .await,
        );
        assert_eq!(waited["state"], "completed", "{waited}");
        assert_eq!(waited["result"]["exit_code"], 0, "{waited}");
        assert!(
            waited["result"]["stdout"]
                .as_str()
                .unwrap()
                .contains("promised"),
            "{waited}"
        );
    }

    #[tokio::test]
    async fn promise_status_does_not_consume_hidden_event() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "echo wake", "mode": "promise"}), &c)
                .await,
        );
        let pid = v["promise_id"].as_str().unwrap().to_string();

        let mut status = json!({"running": true});
        for _ in 0..50 {
            status = c.promises.lock().unwrap().status(&pid).unwrap();
            if status["running"].as_bool() == Some(false) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(status["state"], "completed", "{status}");
        assert_eq!(status["completion_event_delivered"], false, "{status}");

        let events = c.promises.lock().unwrap().drain_completion_events();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(events[0].contains("[HARNESS_EVENT promise_completed]"));
        assert!(events[0].contains(&pid));
        assert!(events[0].contains("call promise_status"));
    }

    #[tokio::test]
    async fn promise_wait_timeout_does_not_consume_hidden_event() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(
                    json!({"command": "sleep 0.05; echo wake", "mode": "promise"}),
                    &c,
                )
                .await,
        );
        let pid = v["promise_id"].as_str().unwrap().to_string();

        let timed_out = as_json(
            crate::promise::PromiseWait
                .call(json!({"promise_id": pid, "timeout_ms": 1}), &c)
                .await,
        );
        assert_eq!(timed_out["timed_out"], true, "{timed_out}");

        let waited = as_json(
            crate::promise::PromiseWait
                .call(json!({"promise_id": pid, "timeout_ms": 3000}), &c)
                .await,
        );
        assert_eq!(waited["state"], "completed", "{waited}");
        assert_eq!(waited["completion_event_delivered"], true, "{waited}");
    }

    #[tokio::test]
    async fn promise_wait_completion_consumes_hidden_event() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "echo wake", "mode": "promise"}), &c)
                .await,
        );
        let pid = v["promise_id"].as_str().unwrap().to_string();

        let waited = as_json(
            crate::promise::PromiseWait
                .call(json!({"promise_id": pid, "timeout_ms": 3000}), &c)
                .await,
        );
        assert_eq!(waited["state"], "completed", "{waited}");
        assert_eq!(waited["completion_event_delivered"], true, "{waited}");
        let events = c.promises.lock().unwrap().drain_completion_events();
        assert!(
            events.is_empty(),
            "promise_wait returned the terminal result, so no stale wake should remain: {events:?}"
        );
    }

    #[tokio::test]
    async fn promise_cancel_stops_running_shell() {
        let c = cx();
        let v = as_json(
            ShellRun
                .call(json!({"command": "sleep 60", "mode": "promise"}), &c)
                .await,
        );
        let pid = v["promise_id"].as_str().unwrap().to_string();
        let cancelled = as_json(
            crate::promise::PromiseCancel
                .call(json!({"promise_id": pid}), &c)
                .await,
        );
        assert_eq!(cancelled["running"], true, "{cancelled}");

        let waited = as_json(
            crate::promise::PromiseWait
                .call(
                    json!({"promise_id": cancelled["promise_id"], "timeout_ms": 3000}),
                    &c,
                )
                .await,
        );
        assert_eq!(waited["state"], "cancelled", "{waited}");
        assert_eq!(waited["completion_event_delivered"], true, "{waited}");
        assert_eq!(waited["result"]["cancelled"], true, "{waited}");
        assert!(c.promises.lock().unwrap().drain_completion_events().is_empty());
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

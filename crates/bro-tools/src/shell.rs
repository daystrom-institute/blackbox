//! Long-running shell support: `shell_run` (spawn + cooperative yield),
//! `shell_poll` (drain / feed / await an existing session), and `shell_kill`
//! (signal + reap a session).
//!
//! Commands return an inline terminal receipt or a retained session id after
//! `yield_time_ms`. Independent supervisors enforce hard deadlines and own each
//! child process group through termination, reaping, and bounded output draining.
//! Polling, feeding stdin, and signalling use shared session handles, so a long
//! poll cannot hide the process from another control call.
//!
//! Sessions last for one harness run. Dropped invocations request cancellation;
//! normal yields leave supervision active. Interrupted turns explicitly call
//! [`shutdown_shell_sessions`] to await final receipts for retained sessions.

mod output;
mod process;
pub(crate) use process::{CommandCapture, run_supervised_command};

use output::OutBuf;

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
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until};

/// Default returned-output budget (~8 KB at a 4-bytes/token heuristic).
const DEFAULT_MAX_OUTPUT_TOKENS: usize = 2_000;
/// Default cooperative yield for a fresh command. Long commands should not make
/// the whole agent turn look hung just because the model forgot to set
/// `yield_time_ms`.
const DEFAULT_RUN_YIELD_MS: u64 = 1_000;
/// Default cooperative yield when polling an already-yielded command. The poll
/// default is longer because the model is explicitly checking an active child.
const DEFAULT_POLL_YIELD_MS: u64 = 5_000;
/// Maximum retained sessions per dispatch, including commands whose final
/// output has not yet been consumed.
const MAX_LIVE_SESSIONS: usize = 32;
/// Grace window for readers to flush final bytes after a child exits, before we
/// abort them. Bounds the case where a grandchild inherited the pipe and holds
/// it open past the direct child's exit (which would otherwise hang forever).
const READER_DRAIN_GRACE: Duration = Duration::from_secs(2);

/// A retained handle never owns the child or blocks process control. The
/// supervisor owns the process from spawn through reaping and reader completion.
struct ShellSession {
    controls: mpsc::UnboundedSender<Control>,
    state: watch::Receiver<Option<TerminalState>>,
    stdin: Arc<tokio::sync::Mutex<Option<ChildStdin>>>,
    stdout: Arc<Mutex<OutBuf>>,
    stderr: Arc<Mutex<OutBuf>>,
    command: String,
    started: Instant,
    hard_deadline: Option<Instant>,
    progress: Arc<PromiseProgress>,
    output_filter: Mutex<Option<ShellOutputFilter>>,
    filter_change: Mutex<Option<Value>>,
}

#[derive(Clone, Default)]
struct TerminalState {
    exit_code: Option<i32>,
    timed_out: bool,
    cancelled: bool,
    killed: bool,
    signal_sent: Option<&'static str>,
    escalated_to_sigkill: bool,
    wait_error: Option<String>,
    group_cleanup_sigkill: bool,
}

enum Control {
    Signal(i32, &'static str),
    Terminate {
        signal: i32,
        name: &'static str,
        deadline: Instant,
    },
    Cancel,
}

/// Covers cancellation even in the interval between spawn and supervisor start.
struct OwnedChild {
    child: Child,
    pgid: u32,
    active: bool,
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.active {
            signal_group(self.pgid, libc::SIGKILL);
        }
    }
}

/// A dropped invocation requests cleanup; a normal yield disarms this guard.
struct InvocationGuard {
    controls: mpsc::UnboundedSender<Control>,
    armed: bool,
}

impl InvocationGuard {
    fn new(session: &ShellSession) -> Self {
        Self {
            controls: session.controls.clone(),
            armed: true,
        }
    }
}

impl Drop for InvocationGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.controls.send(Control::Cancel);
        }
    }
}

/// Entries include exited commands whose final output has not been consumed.
#[derive(Default)]
pub struct ShellSessions {
    map: HashMap<String, Arc<ShellSession>>,
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

    /// Request cleanup without blocking. Use shutdown_shell_sessions when a
    /// caller must await reaping and retain the actual final receipts.
    pub fn shutdown_all(&mut self) -> usize {
        let count = self.map.len();
        for session in self.map.values() {
            let _ = session.controls.send(Control::Cancel);
        }
        self.map.clear();
        count
    }
}

impl Drop for ShellSessions {
    fn drop(&mut self) {
        self.shutdown_all();
    }
}

fn session_progress(session: &ShellSession) -> Value {
    let started_ms =
        crate::promise::now_ms().saturating_sub(session.started.elapsed().as_millis() as u64);
    session.progress.snapshot(started_ms)
}

fn deadline(now: Instant, millis: u64) -> Result<Instant, String> {
    now.checked_add(Duration::from_millis(millis))
        .ok_or_else(|| "requested timeout/yield/grace is too large".to_owned())
}

fn yield_deadline(
    now: Instant,
    requested_ms: Option<u64>,
    default_ms: u64,
) -> Result<Option<Instant>, String> {
    let millis = requested_ms.unwrap_or(default_ms);
    if millis == 0 {
        Ok(None)
    } else {
        deadline(now, millis).map(Some)
    }
}

async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(at) => sleep_until(at).await,
        None => std::future::pending::<()>().await,
    }
}

async fn supervise(
    mut owned: OwnedChild,
    mut controls: mpsc::UnboundedReceiver<Control>,
    state: watch::Sender<Option<TerminalState>>,
    readers: Vec<JoinHandle<()>>,
    kill_at: Option<Instant>,
    cleanup_on_exit: bool,
) {
    let mut terminal = TerminalState::default();
    let mut grace_at = None;
    loop {
        tokio::select! {
            status = owned.child.wait() => {
                match status {
                    Ok(status) => terminal.exit_code = status.code(),
                    Err(error) => terminal.wait_error = Some(error.to_string()),
                }
                break;
            }
            control = controls.recv() => match control {
                Some(Control::Signal(signal, name)) => {
                    signal_group(owned.pgid, signal);
                    terminal.signal_sent = Some(name);
                }
                Some(Control::Terminate { signal, name, deadline }) => {
                    signal_group(owned.pgid, signal);
                    terminal.killed = true;
                    terminal.signal_sent = Some(name);
                    grace_at = Some(grace_at.map_or(deadline, |current: Instant| current.min(deadline)));
                }
                Some(Control::Cancel) | None => {
                    terminal.cancelled = true;
                    signal_group(owned.pgid, libc::SIGKILL);
                    match owned.child.wait().await {
                        Ok(status) => terminal.exit_code = status.code(),
                        Err(error) => terminal.wait_error = Some(error.to_string()),
                    }
                    break;
                }
            },
            _ = until(kill_at) => {
                terminal.timed_out = true;
                signal_group(owned.pgid, libc::SIGKILL);
                match owned.child.wait().await {
                    Ok(status) => terminal.exit_code = status.code(),
                    Err(error) => terminal.wait_error = Some(error.to_string()),
                }
                break;
            }
            _ = until(grace_at) => {
                terminal.escalated_to_sigkill = terminal.signal_sent != Some("kill");
                signal_group(owned.pgid, libc::SIGKILL);
                match owned.child.wait().await {
                    Ok(status) => terminal.exit_code = status.code(),
                    Err(error) => terminal.wait_error = Some(error.to_string()),
                }
                break;
            }
        }
    }
    if terminal.wait_error.is_some() {
        signal_group(owned.pgid, libc::SIGKILL);
        let _ = owned.child.kill().await;
    }
    if cleanup_on_exit || terminal.killed || terminal.cancelled || terminal.timed_out {
        // A shell may exit on TERM while its descendants ignore that signal.
        signal_group(owned.pgid, libc::SIGKILL);
        terminal.group_cleanup_sigkill = true;
    }
    // The direct child can exit while descendants still hold output pipes.
    // Cancellation/deadlines remain effective throughout this final drain.
    let drain = drain_readers(readers);
    tokio::pin!(drain);
    let mut drain_kill_at = if terminal.timed_out { None } else { kill_at };
    let mut controls_open = true;
    loop {
        tokio::select! {
            _ = &mut drain => break,
            control = controls.recv(), if controls_open => {
                match control {
                    Some(Control::Cancel) | None => {
                        terminal.cancelled = true;
                        signal_group(owned.pgid, libc::SIGKILL);
                        terminal.group_cleanup_sigkill = true;
                        controls_open = false;
                    }
                    Some(Control::Signal(signal, name)) => {
                        signal_group(owned.pgid, signal);
                        terminal.signal_sent = Some(name);
                    }
                    Some(Control::Terminate { signal, name, .. }) => {
                        signal_group(owned.pgid, signal);
                        signal_group(owned.pgid, libc::SIGKILL);
                        terminal.killed = true;
                        terminal.signal_sent = Some(name);
                        terminal.group_cleanup_sigkill = true;
                    }
                }
            }
            _ = until(drain_kill_at) => {
                terminal.timed_out = true;
                signal_group(owned.pgid, libc::SIGKILL);
                terminal.group_cleanup_sigkill = true;
                drain_kill_at = None;
            }
        }
    }
    // Linearize completion against concurrent control sends. Closing rejects
    // future sends; recv drains every send accepted before closure, including
    // a stop queued as the final reader became ready.
    controls.close();
    while let Some(control) = controls.recv().await {
        match control {
            Control::Cancel => {
                terminal.cancelled = true;
                signal_group(owned.pgid, libc::SIGKILL);
                terminal.group_cleanup_sigkill = true;
            }
            Control::Signal(signal, name) => {
                signal_group(owned.pgid, signal);
                terminal.signal_sent = Some(name);
            }
            Control::Terminate { signal, name, .. } => {
                signal_group(owned.pgid, signal);
                signal_group(owned.pgid, libc::SIGKILL);
                terminal.killed = true;
                terminal.signal_sent = Some(name);
                terminal.group_cleanup_sigkill = true;
            }
        }
    }
    owned.active = false;
    state.send_replace(Some(terminal));
}

async fn terminal_state(session: &ShellSession) -> TerminalState {
    let mut state = session.state.clone();
    loop {
        if let Some(terminal) = state.borrow().clone() {
            return terminal;
        }
        if state.changed().await.is_err() {
            return TerminalState {
                wait_error: Some("shell supervisor stopped before publishing its outcome".into()),
                ..Default::default()
            };
        }
    }
}

async fn wait_session(session: &ShellSession, yield_at: Option<Instant>, cx: &ToolCx) -> bool {
    let mut state = session.state.clone();
    loop {
        if state.borrow().is_some() {
            return true;
        }
        tokio::select! {
            biased;
            _ = cx.cancellation.cancelled() => {
                let _ = session.controls.send(Control::Cancel);
                terminal_state(session).await;
                return true;
            }
            changed = state.changed() => {
                if changed.is_err() { return true; }
            }
            _ = until(yield_at) => return false,
        }
    }
}

const MAX_STDIN_BYTES: usize = 1024 * 1024;
const MAX_STDIN_WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct InputOutcome {
    written: usize,
    error: Option<String>,
}

async fn feed_stdin(
    session: &ShellSession,
    data: Option<&str>,
    close: bool,
    yield_at: Option<Instant>,
    cx: &ToolCx,
) -> InputOutcome {
    if data.is_none() && !close {
        return InputOutcome::default();
    }
    let mut outcome = InputOutcome::default();
    let mut state = session.state.clone();
    let input_deadline = yield_at.map_or(Instant::now() + MAX_STDIN_WAIT, |at| {
        at.min(Instant::now() + MAX_STDIN_WAIT)
    });
    let write = async {
        let mut stdin = session.stdin.lock().await;
        if let Some(data) = data.filter(|data| !data.is_empty()) {
            let input = stdin.as_mut().ok_or_else(|| "stdin is closed".to_owned())?;
            while outcome.written < data.len() {
                let count = input
                    .write(&data.as_bytes()[outcome.written..])
                    .await
                    .map_err(|error| error.to_string())?;
                if count == 0 {
                    return Err("stdin accepted zero bytes".to_owned());
                }
                outcome.written += count;
            }
            input.flush().await.map_err(|error| error.to_string())?;
        }
        if close {
            stdin.take();
        }
        Ok(())
    };
    let result = tokio::select! {
        biased;
        _ = cx.cancellation.cancelled() => {
            let _ = session.controls.send(Control::Cancel);
            Err("stdin interrupted by invocation cancellation".to_owned())
        }
        result = write => result,
        _ = until(Some(input_deadline)) => Err("stdin write exceeded its wait budget; some input may have been accepted".to_owned()),
        _ = state.changed() => Err("process exited before input was fully accepted".to_owned()),
    };
    outcome.error = result.err();
    outcome
}

fn get_session(cx: &ToolCx, id: &str) -> Result<Arc<ShellSession>, ToolResult> {
    cx.shell_sessions
        .lock()
        .unwrap()
        .map
        .get(id)
        .cloned()
        .ok_or_else(|| {
            ToolResult::Error(format!(
                "no such shell session: {id} (it may have already been consumed)"
            ))
        })
}

fn session_result(
    cx: &ToolCx,
    id: &str,
    session: &ShellSession,
    max_tokens: usize,
    input: InputOutcome,
) -> ToolResult {
    session_result_inner(cx, id, session, max_tokens, input, false)
}

fn session_result_inner(
    cx: &ToolCx,
    id: &str,
    session: &ShellSession,
    max_tokens: usize,
    input: InputOutcome,
    cleanup: bool,
) -> ToolResult {
    let terminal = session.state.borrow().clone().or_else(|| {
        session.state.has_changed().is_err().then(|| TerminalState {
            wait_error: Some("shell supervisor stopped before publishing its outcome".into()),
            ..Default::default()
        })
    });
    let running = terminal.is_none();
    let mut result =
        json!({"session_id": id, "running": running, "exit_code": Value::Null, "timed_out": false});
    if let Some(terminal) = terminal {
        result["exit_code"] = json!(terminal.exit_code);
        result["timed_out"] = json!(terminal.timed_out);
        result["cancelled"] = json!(terminal.cancelled);
        if terminal.group_cleanup_sigkill {
            result["group_cleanup_signal"] = json!("kill");
        }
        if terminal.killed {
            result["killed"] = json!(true);
            result["escalated_to_sigkill"] = json!(terminal.escalated_to_sigkill);
        }
        if let Some(signal) = terminal.signal_sent {
            result["signal_sent"] = json!(signal);
        }
        if let Some(error) = terminal.wait_error {
            result["process_error"] = json!(error.chars().take(160).collect::<String>());
        }
    } else if cx.output_budget == 0 || cx.output_budget >= 4096 {
        result["progress"] = session_progress(session);
    }
    if let Some(error) = input.error {
        result["input_error"] = json!(error);
        result["stdin_bytes_written"] = json!(input.written);
    }
    let result = output_receipt(session, max_tokens, cx.output_budget, result, cleanup);
    if !running && result["output_pending"] == false {
        cx.shell_sessions.lock().unwrap().map.remove(id);
    }
    ToolResult::Json(result)
}

/// Prepare without discarding selected text, fit the complete serialized
/// receipt, then commit exactly the returned strings. Output queue locks cover
/// the preview/commit pair so concurrent polls cannot consume the same bytes.
fn output_receipt(
    session: &ShellSession,
    max_tokens: usize,
    host_budget: usize,
    mut result: Value,
    cleanup: bool,
) -> Value {
    let cap = if host_budget == 0 {
        8192
    } else {
        host_budget.min(8192)
    };
    let requested = max_tokens.saturating_mul(4);
    let filter = session.output_filter.lock().unwrap();
    let mut stdout = session.stdout.lock().unwrap();
    let mut stderr = session.stderr.lock().unwrap();
    let stdout_patterns = filter.as_ref().map_or(&[][..], |f| f.stdout.as_slice());
    let stderr_patterns = filter.as_ref().map_or(&[][..], |f| f.stderr.as_slice());
    stdout.prepare(requested.min(cap), stdout_patterns);
    stderr.prepare(requested.min(cap), stderr_patterns);
    if filter.is_some() {
        let mut report = json!({});
        if !stdout_patterns.is_empty() {
            report["stdout"] = stdout.filter_report();
        }
        if !stderr_patterns.is_empty() {
            report["stderr"] = stderr.filter_report();
        }
        result["output_filter"] = report;
    }
    if let Some(notice) = session.filter_change.lock().unwrap().as_ref() {
        result["filter_change"] = notice.clone();
    }
    let mut per_stream_budget = requested.min(cap / 2);
    let mut compact = false;
    loop {
        let so = stdout.preview(per_stream_budget);
        let se = stderr.preview(per_stream_budget);
        let pending = stdout.pending_bytes() > so.len() || stderr.pending_bytes() > se.len();
        result["stdout"] = json!(so);
        result["stderr"] = json!(se);
        result["output_pending"] = json!(pending && !cleanup);
        result["output"] =
            json!({"stdout": stdout.metadata(so.len()), "stderr": stderr.metadata(se.len())});
        if compact {
            for name in ["stdout", "stderr"] {
                result["output"][name]
                    .as_object_mut()
                    .unwrap()
                    .remove("reader_error");
            }
        }
        if cleanup {
            for name in ["stdout", "stderr"] {
                let count = result["output"][name]["pending_bytes"].as_u64().unwrap();
                if count > 0 {
                    result["output"][name]["discarded_bytes"] = json!(count);
                }
                result["output"][name]["pending_bytes"] = json!(0);
            }
        }
        if !cleanup && !compact && (pending || result["running"] == true) {
            result["next_step"] = json!(
                "Call shell_poll with session_id until running=false and output_pending=false."
            );
        } else {
            result.as_object_mut().unwrap().remove("next_step");
        }
        result
            .as_object_mut()
            .unwrap()
            .remove("minimum_output_bytes");
        if !cleanup && pending && so.is_empty() && se.is_empty() && per_stream_budget < 6 {
            result["minimum_output_bytes"] = json!(6);
        }
        let size = serde_json::to_vec(&result).expect("JSON receipt").len();
        let blocked_by_envelope = requested > 0
            && stdout.selected_bytes() + stderr.selected_bytes() > 0
            && so.is_empty()
            && se.is_empty()
            && per_stream_budget < requested.min(6);
        if size <= cap && !blocked_by_envelope {
            stdout.commit(so.len());
            stderr.commit(se.len());
            if cleanup {
                stdout.discard();
                stderr.discard();
            }
            return result;
        }
        if !compact && (per_stream_budget == 0 || blocked_by_envelope) {
            compact = true;
            result["metadata_omitted"] = json!(true);
            for key in ["progress", "output_filter", "filter_change", "next_step"] {
                result.as_object_mut().unwrap().remove(key);
            }
            for key in ["input_error", "process_error"] {
                if result.as_object_mut().unwrap().remove(key).is_some() {
                    result[format!("{key}_details_omitted")] = json!(true);
                }
            }
            per_stream_budget = requested.min(cap / 2);
            continue;
        }
        if per_stream_budget == 0 {
            // No selected text was consumed. The caller must increase its host
            // envelope budget before receiving this metadata and output.
            let mut fallback = json!({
                "session_id": result["session_id"], "running": result["running"],
                "exit_code": result["exit_code"], "timed_out": result["timed_out"],
                "cancelled": result["cancelled"],
                "output_pending": !cleanup && stdout.pending_bytes() + stderr.pending_bytes() > 0,
                "metadata_omitted": true,
                "minimum_required_bytes": size,
                "error": "shell metadata exceeds the host output budget"
            });
            if cleanup {
                fallback["output"] = json!({
                    "stdout": {"pending_bytes": 0, "discarded_bytes": stdout.discard()},
                    "stderr": {"pending_bytes": 0, "discarded_bytes": stderr.discard()}
                });
                fallback["capture_metadata_omitted"] = json!(true);
            }
            return fallback;
        }
        per_stream_budget =
            per_stream_budget.saturating_sub(size.saturating_sub(cap).div_ceil(2).max(1));
    }
}

/// Stop all sessions owned by this ToolCx, await reaping, and return final facts.
/// The caller can persist these receipts when an interrupted turn had yielded
/// commands whose originating invocation already returned.
pub async fn shutdown_shell_sessions(cx: &ToolCx) -> Vec<Value> {
    let sessions: Vec<_> = cx.shell_sessions.lock().unwrap().map.drain().collect();
    for (_, session) in &sessions {
        let _ = session.controls.send(Control::Cancel);
    }
    let mut receipts = Vec::with_capacity(sessions.len());
    for (id, session) in sessions {
        terminal_state(&session).await;
        if let ToolResult::Json(receipt) = session_result_inner(
            cx,
            &id,
            &session,
            DEFAULT_MAX_OUTPUT_TOKENS,
            InputOutcome::default(),
            true,
        ) {
            receipts.push(receipt);
        }
    }
    receipts
}

struct ReaderCompletion {
    buffer: Arc<Mutex<OutBuf>>,
    finished: bool,
}
impl ReaderCompletion {
    fn finish(&mut self, error: Option<String>) {
        self.buffer.lock().unwrap().finish(error);
        self.finished = true;
    }
}
impl Drop for ReaderCompletion {
    fn drop(&mut self) {
        if !self.finished {
            self.buffer.lock().unwrap().finish(Some(
                "output reader stopped before EOF; unread pipe bytes are unknown".into(),
            ));
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
    let mut completion = ReaderCompletion {
        buffer: buf.clone(),
        finished: false,
    };
    tokio::spawn(async move {
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk).await {
                Ok(0) => {
                    completion.finish(None);
                    break;
                }
                Err(error) => {
                    completion.finish(Some(error.to_string()));
                    break;
                }
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
    stdout: Vec<Regex>,
    stderr: Vec<Regex>,
}

fn update_output_filter(session: &ShellSession, filter: Option<ShellOutputFilter>) {
    let mut current = session.output_filter.lock().unwrap();
    let cached = session.stdout.lock().unwrap().selected_bytes()
        + session.stderr.lock().unwrap().selected_bytes();
    *session.filter_change.lock().unwrap() = Some(json!({
        "scope": "unselected_lines", "cached_bytes_at_change": cached,
    }));
    *current = filter;
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
    Ok(Some(ShellOutputFilter { stdout, stderr }))
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

/// After a child has exited (or been killed) give its readers a bounded grace
/// window to flush remaining bytes, then ABORT stragglers and drain.
///
/// The bound is load-bearing: when a command backgrounds a process that
/// inherited the stdout/stderr pipe (`cmd &`), the direct child exits but the
/// pipe stays open, so a reader awaiting EOF would block forever. We abort it
/// instead of hanging the agent loop.
async fn drain_readers(readers: Vec<JoinHandle<()>>) {
    let drain_at = Instant::now() + READER_DRAIN_GRACE;
    for mut handle in readers {
        if tokio::time::timeout_at(drain_at, &mut handle)
            .await
            .is_err()
        {
            handle.abort();
            let _ = handle.await;
        }
    }
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

fn shell_path_env() -> Option<OsString> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    augmented_path_env(
        std::env::var_os("BRO_EXTRA_PATH"),
        home,
        std::env::var_os("PATH"),
    )
}

/// Apply the standard child-process environment for a non-interactive shell
/// command: augmented PATH, clean/uncolored output, and the host scrub set.
/// Per-command `args.env` is layered on top by the caller and wins.
fn apply_child_env(cmd: &mut tokio::process::Command, cx: &ToolCx) {
    // Non-interactive execution: deterministic, uncolored output for the model.
    cmd.env("NO_COLOR", "1");
    cmd.env("FORCE_COLOR", "0");
    cmd.env("TERM", "dumb");
    if let Some(path) = shell_path_env() {
        cmd.env("PATH", path);
    }
    cx.child_env.apply(cmd.as_std_mut());
    // Host-supplied non-secret overlay (ToolCx::shell_env), applied after the
    // scrub so an explicit host choice is never scrubbed away. Callers apply
    // the model's per-call `env` after this, so the model still wins.
    for (k, v) in cx.shell_env.iter() {
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
    /// default 2000). The TAIL is kept so trailing errors survive.
    max_output_tokens: Option<usize>,
    /// Initial stdin (at most 1 MiB). Writes wait at most 5 seconds or the
    /// invocation yield budget; partial/error writes are reported as input_error.
    /// The stream stays open for
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
        "Run a shell command in the worktree (bash -lc). Returns {exit_code, stdout, stderr, running, timed_out}. Long commands yield by default after ~1s with running=true + session_id; set yield_time_ms to wait that many ms for exit, or 0 to block until exit/timeout. Continue shell_poll until running=false and output_pending=false; completed commands retain unread output. timeout_ms hard-kills a runaway; max_output_tokens caps each stream (default 2000, maximum 3000; prefix pages retain remaining output; zero is metadata-only). output_filter keeps complete matching stdout/stderr lines after capture (lines over 256 KiB are excluded and counted) without changing the real exit_code. stdin feeds initial input; close_stdin sends EOF; env injects variables. Refuses categorically destructive commands."
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
        let max_tokens = args
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
            .min(3_000);

        if args
            .stdin
            .as_ref()
            .is_some_and(|input| input.len() > MAX_STDIN_BYTES)
        {
            return ToolResult::Error(format!(
                "stdin exceeds {MAX_STDIN_BYTES} bytes; write large input to a file and redirect it"
            ));
        }
        let now = Instant::now();
        let yield_at = match yield_deadline(now, args.yield_time_ms, DEFAULT_RUN_YIELD_MS) {
            Ok(at) => at,
            Err(error) => return ToolResult::Error(error),
        };
        let kill_at = match args
            .timeout_ms
            .map(|millis| deadline(now, millis))
            .transpose()
        {
            Ok(at) => at,
            Err(error) => return ToolResult::Error(error),
        };
        if cx.cancellation.is_cancelled() {
            return ToolResult::Error("shell invocation cancelled before spawn".into());
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.args(["-lc", &args.command])
            .current_dir(&cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        apply_child_env(&mut cmd, cx);
        for (key, value) in &args.env {
            cmd.env(key, value);
        }

        // Reserve admission and install ownership without an intervening await.
        // A capacity refusal must not execute a command first.
        let (id, session) = {
            let mut sessions = cx.shell_sessions.lock().unwrap();
            if sessions.map.len() >= MAX_LIVE_SESSIONS {
                return ToolResult::Error(format!(
                    "too many retained shell sessions ({MAX_LIVE_SESSIONS}); poll or stop an existing session first"
                ));
            }
            let child = match cmd.spawn() {
                Ok(child) => child,
                Err(error) => return ToolResult::Error(format!("spawn failed: {error}")),
            };
            let pgid = child.id().expect("freshly spawned child has a pid");
            let mut owned = OwnedChild {
                child,
                pgid,
                active: true,
            };
            let stdout = Arc::new(Mutex::new(OutBuf::default()));
            let stderr = Arc::new(Mutex::new(OutBuf::default()));
            let progress = Arc::new(PromiseProgress::new());
            let mut readers = Vec::new();
            if let Some(output) = owned.child.stdout.take() {
                readers.push(spawn_reader(
                    output,
                    stdout.clone(),
                    Some((StreamKind::Stdout, progress.clone())),
                ));
            }
            if let Some(output) = owned.child.stderr.take() {
                readers.push(spawn_reader(
                    output,
                    stderr.clone(),
                    Some((StreamKind::Stderr, progress.clone())),
                ));
            }
            let stdin = Arc::new(tokio::sync::Mutex::new(owned.child.stdin.take()));
            let (controls, control_rx) = mpsc::unbounded_channel();
            let (state_tx, state) = watch::channel(None);
            let session = Arc::new(ShellSession {
                controls,
                state,
                stdin,
                stdout,
                stderr,
                command: args.command,
                started: now,
                hard_deadline: kill_at,
                progress,
                output_filter: Mutex::new(output_filter),
                filter_change: Mutex::new(None),
            });
            sessions.counter += 1;
            let id = format!("sh-{}", sessions.counter);
            sessions.map.insert(id.clone(), session.clone());
            tokio::spawn(supervise(
                owned, control_rx, state_tx, readers, kill_at, false,
            ));
            (id, session)
        };
        let mut guard = InvocationGuard::new(&session);
        let input = feed_stdin(
            &session,
            args.stdin.as_deref(),
            args.close_stdin,
            yield_at,
            cx,
        )
        .await;
        if cx.cancellation.is_cancelled()
            || session.hard_deadline.is_some_and(|at| at <= Instant::now())
        {
            wait_session(&session, None, cx).await;
        } else if input.error.is_none() {
            wait_session(&session, yield_at, cx).await;
        }
        let mut result = session_result(cx, &id, &session, max_tokens, input);
        if let ToolResult::Json(value) = &mut result
            && value["running"] == false
            && value["output_pending"] == false
        {
            value.as_object_mut().unwrap().remove("session_id");
        }
        guard.armed = false;
        result
    }
}

// ---------------------------------------------------------------------------
// shell_poll
// ---------------------------------------------------------------------------

#[derive(Deserialize, JsonSchema)]
struct ShellPollInput {
    /// Session id from a prior shell_run that returned running=true.
    session_id: String,
    /// Optional stdin (at most 1 MiB); waits at most 5 seconds or the yield
    /// budget. Partial/error writes are reported as input_error.
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
    /// Output token budget for this drain (default 2000, maximum 3000).
    max_output_tokens: Option<usize>,
    /// Optional post-capture line filter for this drain. When omitted, the
    /// filter from the originating shell_run is reused, if any. Changes affect
    /// unselected lines; a previously selected line keeps its page remainder.
    output_filter: Option<ShellOutputFilterInput>,
}

pub struct ShellPoll;

#[async_trait]
impl Tool for ShellPoll {
    fn name(&self) -> &str {
        "shell_poll"
    }
    fn description(&self) -> &str {
        "Resume a running shell session from shell_run: optionally feed stdin, close stdin, send signal=int|term|kill, and wait up to yield_time_ms for exit. Defaults to 5000ms; set yield_time_ms=0 to block until exit/timeout. Returns {exit_code, stdout, stderr, running, timed_out}; running=false means the process exited; keep polling while output_pending=true to receive remaining output. Output pages consume only returned text; overflow retains the newest 8 MiB per stream and reports dropped_bytes. output_filter changes apply to unselected lines; cached page remainders keep their prior selection. Output byte counters describe buffer bytes, not source offsets. If still running, poll again or use shell_kill. The originating timeout_ms still applies."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellPollInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellPollInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let max_tokens = args
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
            .min(3_000);
        let output_filter_arg = args.output_filter;
        let output_filter_was_provided = output_filter_arg.is_some();
        let output_filter = match compile_output_filter(output_filter_arg) {
            Ok(filter) => filter,
            Err(e) => return ToolResult::Error(e),
        };

        if args
            .stdin
            .as_ref()
            .is_some_and(|input| input.len() > MAX_STDIN_BYTES)
        {
            return ToolResult::Error(format!("stdin exceeds {MAX_STDIN_BYTES} bytes"));
        }
        if args
            .signal
            .as_deref()
            .is_some_and(|name| !matches!(name, "int" | "term" | "kill"))
        {
            return ToolResult::Error("signal must be int, term, or kill".into());
        }
        let yield_at =
            match yield_deadline(Instant::now(), args.yield_time_ms, DEFAULT_POLL_YIELD_MS) {
                Ok(at) => at,
                Err(error) => return ToolResult::Error(error),
            };
        let session = match get_session(cx, &args.session_id) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let mut guard = InvocationGuard::new(&session);
        if output_filter_was_provided {
            update_output_filter(&session, output_filter);
        }
        // Signals never queue behind a blocked stdin writer or an output wait.
        if let Some(name) = args.signal.as_deref() {
            let (signal, name) = signal_for(Some(name));
            if session
                .controls
                .send(Control::Signal(signal, name))
                .is_err()
            {
                terminal_state(&session).await;
                let input = if args.stdin.is_some() || args.close_stdin {
                    InputOutcome {
                        written: 0,
                        error: Some("process completed before input was accepted".into()),
                    }
                } else {
                    InputOutcome::default()
                };
                let result = session_result(cx, &args.session_id, &session, max_tokens, input);
                guard.armed = false;
                return result;
            }
        }
        let input = feed_stdin(
            &session,
            args.stdin.as_deref(),
            args.close_stdin,
            yield_at,
            cx,
        )
        .await;
        if cx.cancellation.is_cancelled()
            || session.hard_deadline.is_some_and(|at| at <= Instant::now())
        {
            wait_session(&session, None, cx).await;
        } else if input.error.is_none() {
            wait_session(&session, yield_at, cx).await;
        }
        let result = session_result(cx, &args.session_id, &session, max_tokens, input);
        guard.armed = false;
        result
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
    /// Output token budget for the final drain (default 2000, maximum 3000).
    max_output_tokens: Option<usize>,
    /// Optional post-capture line filter for the final drain. When omitted, the
    /// filter from the originating shell_run is reused, if any. Changes affect
    /// unselected lines; a previously selected line keeps its page remainder.
    output_filter: Option<ShellOutputFilterInput>,
}

pub struct ShellKill;

#[async_trait]
impl Tool for ShellKill {
    fn name(&self) -> &str {
        "shell_kill"
    }
    fn description(&self) -> &str {
        "Terminate a running shell session. Sends signal (term|int|kill, default term), waits up to grace_ms for graceful exit, then force-kills. Returns terminal process facts plus a bounded output page; poll while output_pending=true to finish reading. output_filter can override the originating post-capture line filter for the final drain. Use this to stop a dev server or watch process you started with shell_run + yield_time_ms."
    }
    fn input_schema(&self) -> Value {
        schema_for::<ShellKillInput>()
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let args: ShellKillInput = match serde_json::from_value(input) {
            Ok(a) => a,
            Err(e) => return ToolResult::Error(format!("bad input: {e}")),
        };
        let max_tokens = args
            .max_output_tokens
            .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS)
            .min(3_000);
        let output_filter_arg = args.output_filter;
        let output_filter_was_provided = output_filter_arg.is_some();
        let output_filter = match compile_output_filter(output_filter_arg) {
            Ok(filter) => filter,
            Err(e) => return ToolResult::Error(e),
        };

        if args
            .signal
            .as_deref()
            .is_some_and(|name| !matches!(name, "int" | "term" | "kill"))
        {
            return ToolResult::Error("signal must be int, term, or kill".into());
        }
        let deadline = match deadline(Instant::now(), args.grace_ms.unwrap_or(2000)) {
            Ok(at) => at,
            Err(error) => return ToolResult::Error(error),
        };
        let session = match get_session(cx, &args.session_id) {
            Ok(session) => session,
            Err(error) => return error,
        };
        let mut guard = InvocationGuard::new(&session);
        if output_filter_was_provided {
            update_output_filter(&session, output_filter);
        }
        let (signal, name) = signal_for(args.signal.as_deref());
        let _ = session.controls.send(Control::Terminate {
            signal,
            name,
            deadline,
        });
        wait_session(&session, None, cx).await;
        let result = session_result(
            cx,
            &args.session_id,
            &session,
            max_tokens,
            InputOutcome::default(),
        );
        guard.armed = false;
        result
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
        "List retained shell sessions with session_id, command, elapsed time, and running state. Completed sessions retain unread terminal output until shell_poll returns output_pending=false. Use session_id with shell_poll or shell_kill."
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
                    "running": s.state.borrow().is_none(),
                    "output_pending": s.stdout.lock().unwrap().pending_bytes() > 0 || s.stderr.lock().unwrap().pending_bytes() > 0,
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
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: std::env::temp_dir(),
            safety: Arc::new(crate::safety::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(crate::todo::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(ShellSessions::default())),
            edits: Arc::new(Mutex::new(crate::edits::EditSink::default())),
            child_env: Arc::new(Default::default()),
            cancellation: tokio_util::sync::CancellationToken::new(),
            output_budget: 16 * 1024,
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

    fn isolated_cx() -> (tempfile::TempDir, ToolCx) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut context = cx();
        context.root = root.clone();
        context.shell_env = Arc::new(std::collections::BTreeMap::from([
            ("HOME".to_owned(), root.to_string_lossy().into_owned()),
            (
                "XDG_CONFIG_HOME".to_owned(),
                root.join("config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_STATE_HOME".to_owned(),
                root.join("state").to_string_lossy().into_owned(),
            ),
        ]));
        (directory, context)
    }

    async fn assert_group_stopped(pgid: i32) {
        for _ in 0..100 {
            if !group_has_live_members(pgid) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("process group {pgid} still has live members");
    }

    #[tokio::test]
    async fn queued_cancel_at_ready_terminal_drain_cleans_group_before_publication() {
        let (_directory, c) = isolated_cx();
        let mut command = tokio::process::Command::new("bash");
        command
            .args(["-c", "sleep 30 >/dev/null 2>&1 &"])
            .current_dir(&c.root)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .process_group(0)
            .kill_on_drop(true);
        apply_child_env(&mut command, &c);
        let child = command.spawn().unwrap();
        let pgid = child.id().unwrap();
        let mut owned = OwnedChild {
            child,
            pgid,
            active: true,
        };
        assert!(owned.child.wait().await.unwrap().success());
        assert!(group_has_live_members(pgid as i32));
        let (controls, receiver) = mpsc::unbounded_channel();
        assert!(controls.send(Control::Cancel).is_ok());
        let (sender, receipt) = watch::channel(None);
        // Both child.wait and the empty reader drain are already ready when
        // supervision starts. The accepted cancellation must still be applied.
        supervise(owned, receiver, sender, Vec::new(), None, false).await;
        let terminal = receipt.borrow().clone().unwrap();
        assert_eq!(terminal.exit_code, Some(0));
        assert!(terminal.cancelled);
        assert!(terminal.group_cleanup_sigkill);
        assert!(controls.send(Control::Cancel).is_err());
        assert_group_stopped(pgid as i32).await;
    }

    #[tokio::test]
    async fn hard_deadline_is_enforced_without_polling() {
        let (_directory, c) = isolated_cx();
        let result = as_json(
            ShellRun
                .call(
                    json!({
                        "command": "echo $$ > group; sleep 30 & wait",
                        "yield_time_ms": 25, "timeout_ms": 250
                    }),
                    &c,
                )
                .await,
        );
        assert_eq!(result["running"], true, "{result}");
        // Never poll the session while the hard deadline expires.
        tokio::time::sleep(Duration::from_millis(600)).await;
        let pgid: i32 = std::fs::read_to_string(c.root.join("group"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_group_stopped(pgid).await;
        let receipt = as_json(
            ShellPoll
                .call(
                    json!({"session_id": result["session_id"], "yield_time_ms": 0}),
                    &c,
                )
                .await,
        );
        assert_eq!(receipt["running"], false, "{receipt}");
        assert_eq!(receipt["timed_out"], true, "{receipt}");
        assert_eq!(receipt["cancelled"], false, "{receipt}");
    }

    #[tokio::test]
    async fn nonreading_stdin_is_bounded_and_reports_partial_write() {
        let (_directory, c) = isolated_cx();
        let result = as_json(
            tokio::time::timeout(
                Duration::from_secs(3),
                ShellRun.call(
                    json!({
                        "command": "sleep 30", "stdin": "x".repeat(MAX_STDIN_BYTES),
                        "yield_time_ms": 100, "timeout_ms": 1000
                    }),
                    &c,
                ),
            )
            .await
            .expect("stdin blocked beyond yield budget"),
        );
        assert_eq!(result["running"], true, "{result}");
        assert!(result["input_error"].is_string(), "{result}");
        assert!(result["stdin_bytes_written"].as_u64().unwrap() < MAX_STDIN_BYTES as u64);
        let receipts = shutdown_shell_sessions(&c).await;
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0]["cancelled"], true);
        assert_eq!(receipts[0]["running"], false);
    }

    #[tokio::test]
    async fn nonreading_stdin_hard_timeout_returns_terminal_receipt() {
        let (_directory, c) = isolated_cx();
        let receipt = as_json(
            tokio::time::timeout(
                Duration::from_secs(4),
                ShellRun.call(
                    json!({
                        "command": "sleep 30", "stdin": "x".repeat(MAX_STDIN_BYTES),
                        "yield_time_ms": 0, "timeout_ms": 100
                    }),
                    &c,
                ),
            )
            .await
            .expect("hard timeout did not interrupt stdin"),
        );
        assert_eq!(receipt["running"], false, "{receipt}");
        assert_eq!(receipt["timed_out"], true, "{receipt}");
        assert!(c.shell_sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn active_stdin_cancellation_reaps_process_group_and_returns_receipt() {
        let (_directory, c) = isolated_cx();
        let run = ShellRun.call(
            json!({
                "command": "echo $$ > group; sleep 30 & wait",
                "stdin": "x".repeat(MAX_STDIN_BYTES), "yield_time_ms": 0
            }),
            &c,
        );
        let cancel = async {
            while !c.root.join("group").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            c.cancellation.cancel();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
                .await
                .expect("cancelled stdin did not drain");
        let receipt = as_json(result);
        assert_eq!(receipt["running"], false, "{receipt}");
        assert_eq!(receipt["cancelled"], true, "{receipt}");
        assert!(receipt["input_error"].is_string(), "{receipt}");
        let pgid: i32 = std::fs::read_to_string(c.root.join("group"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_group_stopped(pgid).await;
        assert!(c.shell_sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn long_poll_remains_reachable_to_list_and_kill() {
        let (_directory, c) = isolated_cx();
        let initial = as_json(
            ShellRun
                .call(json!({"command": "sleep 30", "yield_time_ms": 20}), &c)
                .await,
        );
        let id = initial["session_id"].as_str().unwrap();
        let poll = ShellPoll.call(json!({"session_id": id, "yield_time_ms": 0}), &c);
        let kill = async {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let listed = as_json(ShellList.call(json!({}), &c).await);
            assert_eq!(listed["sessions"][0]["session_id"], id, "{listed}");
            ShellKill
                .call(json!({"session_id": id, "signal": "kill"}), &c)
                .await
        };
        let (polled, killed) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(poll, kill) })
                .await
                .expect("poll hid session from kill");
        for receipt in [as_json(polled), as_json(killed)] {
            assert_eq!(receipt["running"], false, "{receipt}");
            assert_eq!(receipt["killed"], true, "{receipt}");
        }
    }

    #[tokio::test]
    async fn term_cleanup_kills_descendant_that_ignores_term() {
        let (_directory, c) = isolated_cx();
        let initial = as_json(ShellRun.call(json!({
            "command": "echo $$ > group; (trap '' TERM; echo ready > ready; sleep 30; echo survived > sentinel) & wait",
            "yield_time_ms": 20
        }), &c).await);
        tokio::time::timeout(Duration::from_secs(3), async {
            while !c.root.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let killed = as_json(
            ShellKill
                .call(
                    json!({"session_id": initial["session_id"], "signal": "term"}),
                    &c,
                )
                .await,
        );
        assert_eq!(killed["running"], false, "{killed}");
        assert_eq!(killed["group_cleanup_signal"], "kill", "{killed}");
        let pgid: i32 = std::fs::read_to_string(c.root.join("group"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_group_stopped(pgid).await;
        assert!(!c.root.join("sentinel").exists());
    }

    #[tokio::test]
    async fn cancellation_during_reader_drain_stops_background_group() {
        let (_directory, c) = isolated_cx();
        let run = ShellRun.call(
            json!({"command": "echo $$ > group; sleep 30 &", "yield_time_ms": 0}),
            &c,
        );
        let cancel = async {
            while !c.root.join("group").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
            c.cancellation.cancel();
        };
        let (result, ()) =
            tokio::time::timeout(Duration::from_secs(5), async { tokio::join!(run, cancel) })
                .await
                .unwrap();
        let receipt = as_json(result);
        assert_eq!(receipt["cancelled"], true, "{receipt}");
        assert_eq!(receipt["running"], false, "{receipt}");
        let pgid: i32 = std::fs::read_to_string(c.root.join("group"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert_group_stopped(pgid).await;
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
    async fn spawned_shell_calls_keep_scrub_policy_and_explicit_env_precedence() {
        let dir = tempfile::tempdir().unwrap();
        let mut cx = cx();
        cx.root = dir.path().canonicalize().unwrap();
        // ShellRun installs this known non-secret value before the scrub. It
        // exercises real shell dispatch without mutating the test process env.
        cx.child_env = Arc::new(crate::ChildEnvironment::new(["NO_COLOR".into()]));
        let results = tokio::spawn(async move {
            let input = json!({"command": "printf '%s' \"${NO_COLOR-unset}\"", "yield_time_ms": 0});
            let scrubbed = as_json(ShellRun.call(input.clone(), &cx).await);
            cx.shell_env = Arc::new(std::collections::BTreeMap::from([(
                "NO_COLOR".into(),
                "explicit-host".into(),
            )]));
            let host = as_json(ShellRun.call(input, &cx).await);
            let model = as_json(
                ShellRun
                    .call(
                        json!({
                            "command": "printf '%s' \"$NO_COLOR\"",
                            "yield_time_ms": 0,
                            "env": {"NO_COLOR": "explicit-call"}
                        }),
                        &cx,
                    )
                    .await,
            );
            (scrubbed, host, model)
        })
        .await
        .unwrap();
        assert_eq!(results.0["stdout"], "unset");
        assert_eq!(results.1["stdout"], "explicit-host");
        assert_eq!(results.2["stdout"], "explicit-call");
    }

    #[tokio::test]
    async fn shell_children_get_uncolored_output_env() {
        // apply_child_env sets NO_COLOR when no session policy removes it.
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
                if !entry
                    .file_name()
                    .to_string_lossy()
                    .bytes()
                    .all(|b| b.is_ascii_digit())
                {
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
    async fn output_pages_preserve_complete_stream_under_escaped_host_budget() {
        let (_dir, mut c) = isolated_cx();
        c.output_budget = 768;
        let expected = "\"\\\n😀".repeat(500);
        std::fs::write(c.root.join("output"), &expected).unwrap();
        let mut receipt = as_json(
            ShellRun
                .call(
                    json!({"command": "cat output; cat output >&2", "yield_time_ms": 0}),
                    &c,
                )
                .await,
        );
        let mut stdout = String::new();
        let mut stderr = String::new();
        for _ in 0..200 {
            assert!(
                serde_json::to_vec(&receipt).unwrap().len() <= c.output_budget,
                "{receipt}"
            );
            assert_eq!(receipt["running"], false);
            stdout.push_str(receipt["stdout"].as_str().unwrap());
            stderr.push_str(receipt["stderr"].as_str().unwrap());
            if receipt["output_pending"] == false {
                break;
            }
            receipt = as_json(
                ShellPoll
                    .call(
                        json!({"session_id": receipt["session_id"], "yield_time_ms": 0}),
                        &c,
                    )
                    .await,
            );
        }
        assert_eq!(stdout, expected);
        assert_eq!(stderr, expected);
        assert!(c.shell_sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn zero_output_budget_keeps_terminal_output_for_later_poll() {
        let (_dir, c) = isolated_cx();
        let first = as_json(
            ShellRun
                .call(
                    json!({"command": "printf abc", "yield_time_ms": 0, "max_output_tokens": 0}),
                    &c,
                )
                .await,
        );
        assert_eq!(first["running"], false);
        assert_eq!(first["stdout"], "");
        assert_eq!(first["output_pending"], true);
        assert_eq!(first["output"]["stdout"]["pending_bytes"], 3);
        let last = as_json(
            ShellPoll
                .call(
                    json!({"session_id": first["session_id"], "max_output_tokens": 2}),
                    &c,
                )
                .await,
        );
        assert_eq!(last["stdout"], "abc");
        assert_eq!(last["output_pending"], false);
    }

    #[tokio::test]
    async fn filter_change_reports_cached_page_remainder_as_prior_selection() {
        let (_dir, mut c) = isolated_cx();
        c.output_budget = 768;
        let text = format!("{}\nkeep next\n", "noise ".repeat(200));
        std::fs::write(c.root.join("output"), text).unwrap();
        let first = as_json(
            ShellRun
                .call(json!({"command": "cat output", "yield_time_ms": 0}), &c)
                .await,
        );
        let second = as_json(ShellPoll.call(json!({"session_id": first["session_id"], "output_filter": {"stdout": "^keep"}}), &c).await);
        assert_eq!(second["filter_change"]["scope"], "unselected_lines");
        assert!(
            second["filter_change"]["cached_bytes_at_change"]
                .as_u64()
                .unwrap()
                > 0
        );
        assert!(second["stdout"].as_str().unwrap().contains("noise"));
        shutdown_shell_sessions(&c).await;
    }

    #[tokio::test]
    async fn shutdown_reports_unreturned_output_as_discarded_within_budget() {
        let (_dir, mut c) = isolated_cx();
        c.output_budget = 768;
        std::fs::write(c.root.join("output"), "x".repeat(10000)).unwrap();
        let first = as_json(
            ShellRun
                .call(
                    json!({"command": "cat output", "yield_time_ms": 0, "max_output_tokens": 0}),
                    &c,
                )
                .await,
        );
        assert_eq!(first["output_pending"], true);
        let receipts = shutdown_shell_sessions(&c).await;
        assert_eq!(receipts.len(), 1);
        let last = &receipts[0];
        assert!(
            serde_json::to_vec(last).unwrap().len() <= c.output_budget,
            "{last}"
        );
        assert_eq!(last["output_pending"], false);
        let returned = last["stdout"].as_str().unwrap().len();
        let discarded = last["output"]["stdout"]["discarded_bytes"]
            .as_u64()
            .unwrap() as usize;
        assert_eq!(returned + discarded, 10000);
        assert!(c.shell_sessions.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn compact_metadata_makes_progress_and_releases_empty_terminal_session() {
        for has_output in [false, true] {
            let (_dir, mut c) = isolated_cx();
            c.output_budget = 768;
            let first = as_json(ShellRun.call(json!({"command": "printf retained", "yield_time_ms": 0, "max_output_tokens": 0}), &c).await);
            let id = first["session_id"].as_str().unwrap();
            let session = get_session(&c, id).unwrap();
            for buffer in [&session.stdout, &session.stderr] {
                let mut buffer = buffer.lock().unwrap();
                buffer.push(&[0xff]);
                buffer.finish(Some("\u{1}".repeat(96)));
                buffer.prepare(32, &[]);
                if !has_output {
                    buffer.discard();
                }
            }
            let receipt = as_json(ShellPoll.call(json!({"session_id": id}), &c).await);
            assert!(
                serde_json::to_vec(&receipt).unwrap().len() <= 768,
                "{receipt}"
            );
            assert_eq!(receipt["metadata_omitted"], true, "{receipt}");
            assert_eq!(receipt["output"]["stdout"]["capture_incomplete"], true);
            assert_eq!(receipt["output"]["stderr"]["capture_incomplete"], true);
            assert_eq!(receipt["output_pending"], false, "{receipt}");
            assert_eq!(receipt["stdout"], if has_output { "retained�" } else { "" });
            assert_eq!(receipt["stderr"], if has_output { "�" } else { "" });
            assert_eq!(receipt["output"]["stdout"]["invalid_utf8_bytes"], 1);
            assert_eq!(receipt["output"]["stderr"]["invalid_utf8_bytes"], 1);
            assert!(c.shell_sessions.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn reader_failure_is_reported_and_incomplete_utf8_is_finalized() {
        struct Broken(bool);
        impl tokio::io::AsyncRead for Broken {
            fn poll_read(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                buf: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                if !self.0 {
                    self.0 = true;
                    buf.put_slice(&[0xe2]);
                    return std::task::Poll::Ready(Ok(()));
                }
                std::task::Poll::Ready(Err(std::io::Error::other("synthetic reader failure")))
            }
        }
        let buffer = Arc::new(Mutex::new(OutBuf::default()));
        spawn_reader(Broken(false), buffer.clone(), None)
            .await
            .unwrap();
        let mut buffer = buffer.lock().unwrap();
        buffer.prepare(8, &[]);
        assert_eq!(buffer.preview(8), "�");
        assert_eq!(buffer.metadata(0)["capture_incomplete"], true);
        assert_eq!(buffer.metadata(0)["invalid_utf8_bytes"], 1);
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
    #[tokio::test]
    async fn internal_command_preserves_raw_bytes_and_bounds_both_streams() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut cx = cx();
        cx.root = root.clone();
        let mut command = tokio::process::Command::new("bash");
        command.env_clear().env("PATH", "/usr/bin:/bin").env("HOME", &root).current_dir(&root)
            .args(["-c", "printf '\\377\\000A'; for i in {1..100}; do printf x; printf y >&2; done; printf FINAL >&2"]);
        let result = run_supervised_command(command, &cx, Duration::from_secs(2), 16, 16)
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        assert_eq!(&result.stdout[..3], &[255, 0, b'A']);
        assert_eq!(result.stdout.len(), 16);
        assert_eq!(result.stdout_dropped, 87);
        assert_eq!(result.stderr.len(), 16);
        assert!(result.stderr.ends_with(b"FINAL"));
        assert_eq!(result.stderr_dropped, 89);
        assert!(!result.complete());
    }

    #[tokio::test]
    async fn internal_command_timeout_and_cancel_reap_owned_group() {
        for cancel in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let mut cx = cx();
            cx.root = root.clone();
            let mut command = tokio::process::Command::new("bash");
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &root)
                .current_dir(&root)
                .args(["-c", "(sleep 0.4; printf leaked > sentinel) & wait"]);
            let cancellation = cx.cancellation.clone();
            let cancel_task = tokio::spawn(async move {
                if cancel {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    cancellation.cancel();
                }
            });
            let timeout = if cancel {
                Duration::from_secs(5)
            } else {
                Duration::from_millis(50)
            };
            let result = run_supervised_command(command, &cx, timeout, 100, 100)
                .await
                .unwrap();
            cancel_task.await.unwrap();
            assert_eq!(result.cancelled, cancel);
            assert_eq!(result.timed_out, !cancel);
            tokio::time::sleep(Duration::from_millis(450)).await;
            assert!(!root.join("sentinel").exists());
        }
    }

    #[tokio::test]
    async fn internal_finite_command_cleans_background_children_after_success() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let mut cx = cx();
        cx.root = root.clone();
        let mut command = tokio::process::Command::new("bash");
        command
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &root)
            .current_dir(&root)
            .args(["-c", "(sleep 0.3; printf leaked > sentinel) & exit 0"]);
        let result = run_supervised_command(command, &cx, Duration::from_secs(2), 100, 100)
            .await
            .unwrap();
        assert_eq!(result.exit_code, Some(0));
        tokio::time::sleep(Duration::from_millis(350)).await;
        assert!(!root.join("sentinel").exists());
    }
}

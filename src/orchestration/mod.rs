pub mod brofile;
pub mod mcp;
pub mod providers;
pub mod tail;
pub mod team;

use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::process::Command;
use tokio::sync::Notify;

use providers::{EventSink, Provider, Usage};

// ---------------------------------------------------------------------------
// Task
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

impl TaskStatus {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            TaskStatus::Completed | TaskStatus::Failed | TaskStatus::Cancelled
        )
    }
}

/// Shared inner state of a task, updated by background readers.
pub struct TaskInner {
    pub id: String,
    pub provider: Provider,
    pub session_id: String,
    pub events: Vec<Value>,
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub stderr: String,
    pub status: TaskStatus,
    pub started_at: u64,
    pub completed_at: Option<u64>,
    pub exit_code: Option<i32>,
    pub cwd: Option<String>,
}

pub struct Task {
    pub inner: Mutex<TaskInner>,
    pub notify: Arc<Notify>,
    /// Handle to the child process for cancellation. Only set while running.
    child_id: Mutex<Option<u32>>, // PID
}

impl Task {
    pub fn id(&self) -> String {
        self.inner.lock().id.clone()
    }
}

// ---------------------------------------------------------------------------
// Task Store
// ---------------------------------------------------------------------------

pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).cloned()
    }

    pub fn insert(&mut self, id: String, task: Arc<Task>) {
        self.tasks.insert(id, task);
    }

    pub fn all_tasks(&self) -> Vec<Arc<Task>> {
        self.tasks.values().cloned().collect()
    }

    /// Drop entries matching the predicate (e.g. failed, older than X).
    /// Returns the IDs that were removed for caller reporting + persist.
    pub fn retain_drop<F>(&mut self, mut keep: F) -> Vec<String>
    where
        F: FnMut(&Task) -> bool,
    {
        let mut dropped = Vec::new();
        self.tasks.retain(|id, t| {
            if keep(t) {
                true
            } else {
                dropped.push(id.clone());
                false
            }
        });
        dropped
    }
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

const MAX_PERSISTED_EVENTS: usize = 50;

#[derive(Serialize, Deserialize)]
struct PersistedTask {
    id: String,
    provider: Provider,
    session_id: String,
    events: Vec<Value>,
    last_assistant_message: Option<String>,
    usage: Option<Usage>,
    cost_usd: Option<f64>,
    num_turns: Option<u64>,
    stderr: String,
    status: TaskStatus,
    started_at: u64,
    completed_at: Option<u64>,
    exit_code: Option<i32>,
    cwd: Option<String>,
}

impl TaskStore {
    pub fn persist(&self, store_dir: &std::path::Path) {
        let records: Vec<PersistedTask> = self
            .tasks
            .values()
            .map(|t| {
                let inner = t.inner.lock();
                PersistedTask {
                    id: inner.id.clone(),
                    provider: inner.provider,
                    session_id: inner.session_id.clone(),
                    events: inner
                        .events
                        .iter()
                        .rev()
                        .take(MAX_PERSISTED_EVENTS)
                        .rev()
                        .cloned()
                        .collect(),
                    last_assistant_message: inner.last_assistant_message.clone(),
                    usage: inner.usage.clone(),
                    cost_usd: inner.cost_usd,
                    num_turns: inner.num_turns,
                    stderr: inner.stderr.chars().take(2000).collect(),
                    status: inner.status,
                    started_at: inner.started_at,
                    completed_at: inner.completed_at,
                    exit_code: inner.exit_code,
                    cwd: inner.cwd.clone(),
                }
            })
            .collect();

        let file = store_dir.join("tasks.json");
        let tmp = store_dir.join("tasks.json.tmp");
        if let Ok(data) = serde_json::to_string(&records) {
            let _ = std::fs::create_dir_all(store_dir);
            if std::fs::write(&tmp, &data).is_ok() {
                let _ = std::fs::rename(&tmp, &file);
            }
        }
    }

    pub fn load(store_dir: &std::path::Path, ttl_ms: u64) -> Self {
        let file = store_dir.join("tasks.json");
        let mut store = Self::new();
        let data = match std::fs::read_to_string(&file) {
            Ok(d) => d,
            Err(_) => return store,
        };
        let records: Vec<PersistedTask> = match serde_json::from_str(&data) {
            Ok(r) => r,
            Err(_) => return store,
        };
        let cutoff = now_ms().saturating_sub(ttl_ms);
        for mut rec in records {
            if rec.started_at < cutoff {
                continue;
            }
            if rec.status == TaskStatus::Running {
                rec.status = TaskStatus::Failed;
                rec.completed_at = Some(now_ms());
                rec.stderr
                    .push_str("\n[blackbox] server restarted while task was running");
            }
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: rec.id.clone(),
                    provider: rec.provider,
                    session_id: rec.session_id,
                    events: rec.events,
                    last_assistant_message: rec.last_assistant_message,
                    usage: rec.usage,
                    cost_usd: rec.cost_usd,
                    num_turns: rec.num_turns,
                    stderr: rec.stderr,
                    status: rec.status,
                    started_at: rec.started_at,
                    completed_at: rec.completed_at,
                    exit_code: rec.exit_code,
                    cwd: rec.cwd,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            store.insert(rec.id, task);
        }
        store
    }
}

// ---------------------------------------------------------------------------
// Spawn + lifecycle
// ---------------------------------------------------------------------------

// ── Ambient prompt layer (per-turn, scoping + guardrails) ───────────
//
// The per-turn injection carries only what the agent cannot otherwise
// derive: guardrails (recursion guard) and pre-bound scoping IDs
// (session, project, bro, thread, work-item). It does NOT carry tool
// vocabulary or protocol definitions — those belong to the start-of-
// session layer rendered from `tool_docs` into the global memory files.
//
// This is deliberately separate from the brofile lens (persona / role
// system-prompt). `apply_ambient` and `apply_brofile_lens` compose
// freely but have distinct responsibilities:
//   - ambient  = guardrail + scope (daemon-controlled, every dispatch)
//   - lens     = persona / system-prompt (user-authored, per brofile)

// Text recursion guard retired 2026-04-17. Every dispatch-capable
// provider (Claude, Copilot, Codex, Gemini) now has a mechanical tool
// filter applied at argv construction time. Vibe has no MCP at all, so
// no bro_* tools reach it to recurse through.
//
// If defense-in-depth text guards are wanted in the future, reintroduce
// a prefix here and gate on `AmbientContext::provider`.

/// Per-turn recall directive. The managed-region CORE RULE reliably
/// triggers `bbox_knowledge` queries on cold-start but attention-
/// decays within-session on Claude Opus 4.7 and Gemini 2.5-flash:
/// at ~15 turns of accumulated context, the session-start memory
/// guidance no longer binds. Per-turn ambient injection survives
/// because it rides with every turn. Codex gpt-5.4 does not appear
/// to need this — Tier 1 instruction binds at depth there — but
/// the extra reinforcement is harmless. Parallels the empirical
/// fix for `bbox_note` emission (DEFAULT_COMPLETION_CONTRACT below).
pub const RECALL_DIRECTIVE: &str = "\
Recall: your FIRST tool call on any substantive task must be \
`bbox_knowledge(query=<one keyword>)`. Justification and fallback \
procedure are in the managed tool reference.";

/// Default per-dispatch contract requiring a structured completion
/// signal before the agent returns. Observed empirically: without
/// this, agents competently complete tasks via prose but never emit
/// `bbox_note(kind="done")` on their own, even when global docs
/// describe the protocol and they can articulate it back. The
/// contract converts soft doc guidance into a per-turn requirement.
pub const DEFAULT_COMPLETION_CONTRACT: &str = "\
REQUIRED — before returning your final answer, emit bbox_note records:\n\
\n\
1. Emit a SEPARATE `mcp__blackbox__bbox_note` call for each distinct \
finding that arose during the work. Do NOT consolidate multiple findings \
into one body. Use the right kind for each:\n\
   • `surprise` — concrete instance where you expected X and found Y \
(e.g., \"expected the escape fn to handle control chars, found it only \
escapes backslash+quote\"). Emit one per surprise.\n\
   • `followup` — concrete out-of-scope work you noticed but did not do \
(e.g., \"add roundtrip TOML parse test for format_toml_string_array\"). \
One per followup. Do NOT do the work — just record.\n\
   • `assumption` — ambiguity-resolving judgment you made to proceed \
(e.g., \"brief said `blackbox` server but repo has multiple; assumed the \
one in src/orchestration/\"). One per assumption.\n\
   • `learned` — project-local convention you discovered in situ (e.g., \
\"repo uses `bb:managed-start` markers, not editable outside\").\n\
   • `blocked`, `dispute` — as applicable.\n\
Consolidating multiple findings into one body is only correct when they \
are genuinely one idea. If your prose response has bullet points of \
findings, those are almost always separate notes.\n\
\n\
2. Finally, emit a `done` note with a one-line summary. Not generic \
phrases like \"task complete\" — something concrete like \"verified X \
already handles Y\" or \"audited Z; 3 concerns flagged via separate \
notes\".\n\
\n\
Every call MUST include:\n\
  task_id=<copy `task:` from [scope] above EXACTLY — do NOT paste any \
other value (not project path, not prose, not \"pending\") into this field>\n\
  project=<`project` from [scope], if present>\n\
  bro=<`bro` from [scope], if present>\n\
  session_id=<`session` from [scope], if present>\n\
The task_id is the primary correlation key and the orchestrator uses it \
to find your notes.";

/// Pre-bound context the daemon has at dispatch time but the executor
/// would otherwise have to infer by reaching back through the prompt.
/// Emitting these into the prefix lets notes, thread links, and work-
/// item attribution land correctly on the first attempt.
#[derive(Debug, Clone, Default)]
pub struct AmbientContext {
    /// Daemon-generated dispatch task ID. Stable pre-spawn and across
    /// all providers, regardless of when each provider emits its own
    /// session ID. Used as the primary correlation key for notes:
    /// agents copy the `task:` scope value into `bbox_note.task_id`.
    pub task_id: Option<String>,
    pub session_id: Option<String>,
    pub project_dir: Option<String>,
    pub bro_name: Option<String>,
    pub thread_id: Option<String>,
    pub work_item_id: Option<String>,
    /// Per-dispatch expectation, e.g. "call bbox_note(kind='done', body='…') before returning".
    pub completion_contract: Option<String>,
    pub allow_recursion: bool,
    /// Target provider. When set and the provider supports dispatch-time
    /// tool filtering (Claude/Copilot), the text recursion guard is
    /// omitted in favor of the mechanical filter applied at the CLI arg
    /// layer. Unset or unsupported provider → text guard as fallback.
    pub provider: Option<providers::Provider>,
}

impl AmbientContext {
    /// Pending session IDs (non-Claude providers before the CLI emits
    /// one) carry no useful linkage — omit rather than leak the literal
    /// "pending" into the prefix.
    fn session_field(&self) -> Option<&str> {
        match self.session_id.as_deref() {
            Some("pending") | Some("") | None => None,
            Some(s) => Some(s),
        }
    }

    fn scope_fields(&self) -> Vec<String> {
        let mut parts = Vec::new();
        // task ID comes first — it's the stable correlation key and
        // agents should always have it. Contract tells them to copy
        // it into bbox_note.task_id.
        if let Some(t) = &self.task_id {
            parts.push(format!("task: {t}"));
        }
        if let Some(s) = self.session_field() {
            parts.push(format!("session: {s}"));
        }
        if let Some(p) = &self.project_dir {
            parts.push(format!("project: {p}"));
        }
        if let Some(b) = &self.bro_name {
            parts.push(format!("bro: {b}"));
        }
        if let Some(t) = &self.thread_id {
            parts.push(format!("thread: {t}"));
        }
        if let Some(w) = &self.work_item_id {
            parts.push(format!("work_item: {w}"));
        }
        parts
    }
}

/// Wrap a prompt with the per-turn ambient prefix (scope block +
/// optional completion contract). Skipped entirely when
/// `allow_recursion` is set. Does NOT touch the brofile lens.
///
/// Recursion guarding is done mechanically via provider-specific tool-
/// filter args (`--disallowedTools`, `--deny-tool`, `-c disabled_tools=…`,
/// or `--policy <file>`), appended to argv outside this function. No
/// text guard is emitted.
pub fn apply_ambient(prompt: &str, ctx: &AmbientContext) -> String {
    if ctx.allow_recursion {
        return prompt.to_string();
    }
    let mut prefix = String::new();

    let fields = ctx.scope_fields();
    if !fields.is_empty() {
        prefix.push_str("[scope] ");
        prefix.push_str(&fields.join(" · "));
        prefix.push_str("\n\n");
    }

    // Per-turn recall reinforcement. Session-start memory guidance
    // decays at depth on Claude and Gemini; ambient survives because
    // it rides with every turn.
    prefix.push_str("[recall before acting]\n");
    prefix.push_str(RECALL_DIRECTIVE);
    prefix.push_str("\n\n");

    if let Some(contract) = &ctx.completion_contract {
        prefix.push_str("[completion contract]\n");
        prefix.push_str(contract.trim_end());
        prefix.push_str("\n\n");
    }

    format!("{prefix}{prompt}")
}

/// Prepend the brofile lens (persona / system prompt) to a prompt.
/// Kept deliberately separate from `apply_ambient` — they're orthogonal
/// layers. Compose: `apply_brofile_lens(&apply_ambient(p, &ctx), lens)`.
pub fn apply_brofile_lens(prompt: &str, lens: Option<&str>) -> String {
    match lens {
        Some(l) if !l.trim().is_empty() => format!("{l}\n\n{prompt}"),
        _ => prompt.to_string(),
    }
}

/// Spawn a provider CLI process and return a tracked Task.
///
/// `task_id` is pre-generated by the caller so it can be threaded into
/// the ambient `[scope]` block before the subprocess launches. That lets
/// agents emit `bbox_note(task_id=...)` records correlated back to the
/// dispatch regardless of when the provider emits its own session ID.
#[allow(clippy::too_many_arguments)]
pub fn spawn_task(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
) -> Arc<Task> {
    let id = task_id;

    let extra_path = std::env::var("BRO_EXTRA_PATH").unwrap_or_else(|_| {
        dirs::home_dir()
            .unwrap_or_default()
            .join(".local/bin")
            .to_string_lossy()
            .to_string()
    });
    let path_env = format!(
        "{}:{}",
        extra_path,
        std::env::var("PATH").unwrap_or_default()
    );

    // Resolve binary through a login shell so nvm/asdf/rbenv-installed CLIs
    // work even when the daemon was launched by launchctl/systemd with a
    // narrow PATH. Falls back to the bare name, which preserves the
    // existing error surface when the binary genuinely is not installed.
    let raw_bin = provider.bin();
    let bin = providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .env("PATH", &path_env)
        .env("NO_COLOR", "1")
        .env("TERM", "dumb")
        .env("FORCE_COLOR", "0");

    if let Some(ref c) = cwd {
        cmd.current_dir(c);
    }
    if let Some(ref overrides) = env_overrides {
        for (k, v) in overrides {
            cmd.env(k, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Return a failed task immediately
            let task = Arc::new(Task {
                inner: Mutex::new(TaskInner {
                    id: id.clone(),
                    provider,
                    session_id,
                    events: vec![],
                    last_assistant_message: None,
                    usage: None,
                    cost_usd: None,
                    num_turns: None,
                    stderr: format!("spawn error: {e}"),
                    status: TaskStatus::Failed,
                    started_at: now_ms(),
                    completed_at: Some(now_ms()),
                    exit_code: None,
                    cwd,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            task_store.write().insert(id, task.clone());
            task_store.read().persist(&store_dir);
            task.notify.notify_waiters();
            return task;
        }
    };

    let pid = child.id();
    let task = Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: id.clone(),
            provider,
            session_id: session_id.clone(),
            events: vec![],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: now_ms(),
            completed_at: None,
            exit_code: None,
            cwd: cwd.clone(),
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(pid),
    });

    task_store.write().insert(id.clone(), task.clone());

    // Emit tail event
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        task_id: id.clone(),
        provider,
        bro_name: None,
    });

    if provider == Provider::Gemini && session_id == "pending" {
        if let Some(cwd_clone) = cwd.clone() {
            let task_ref_watch = task.clone();
            let task_store_watch = task_store.clone();
            let tail_tx_watch = tail_tx.clone();
            let store_dir_watch = store_dir.clone();
            let task_id_watch = id.clone();
            let started_at = task.inner.lock().started_at;
            tokio::spawn(async move {
                loop {
                    {
                        let inner = task_ref_watch.inner.lock();
                        if inner.status != TaskStatus::Running || inner.session_id != "pending" {
                            break;
                        }
                    }

                    if let Some(sid) = providers::discover_gemini_session(started_at, &cwd_clone) {
                        let discovered = {
                            let mut inner = task_ref_watch.inner.lock();
                            if inner.session_id == "pending" {
                                inner.session_id = sid.clone();
                                true
                            } else {
                                false
                            }
                        };
                        if discovered {
                            team::propagate_session_id(&task_id_watch, &sid, &store_dir_watch);
                            task_store_watch.read().persist(&store_dir_watch);
                            let _ = tail_tx_watch.send(tail::TailEvent::TaskProgress {
                                task_id: task_id_watch.clone(),
                                activity: String::new(),
                            });
                        }
                        break;
                    }

                    tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                }
            });
        }
    }

    // Spawn stdout reader — signals completion via oneshot so the process
    // waiter can ensure all output is consumed before marking the task done.
    let stdout = child.stdout.take().unwrap();
    let stderr_handle = child.stderr.take().unwrap();
    let task_ref = task.clone();
    let is_streaming = provider.is_streaming_json();
    let tail_tx_clone = tail_tx.clone();
    let task_id_clone = id.clone();

    let (stdout_done_tx, stdout_done_rx) = tokio::sync::oneshot::channel::<()>();

    if is_streaming {
        // Line-by-line JSON parsing
        tokio::spawn(async move {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut last_emitted_snippet: Option<String> = None;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Ok(evt) = serde_json::from_str::<Value>(&line) {
                    let snippet_to_emit = {
                        let mut inner = task_ref.inner.lock();
                        inner.events.push(evt.clone());
                        let mut sink = EventSink {
                            last_assistant_message: inner.last_assistant_message.clone(),
                            usage: inner.usage.clone(),
                            cost_usd: inner.cost_usd,
                            num_turns: inner.num_turns,
                            session_id: if inner.session_id != "pending" {
                                Some(inner.session_id.clone())
                            } else {
                                None
                            },
                        };
                        provider.parse_event(&evt, &mut sink);
                        inner.last_assistant_message = sink.last_assistant_message;
                        inner.usage = sink.usage;
                        inner.cost_usd = sink.cost_usd;
                        inner.num_turns = sink.num_turns;
                        if let Some(sid) = sink.session_id {
                            if inner.session_id == "pending" {
                                inner.session_id = sid;
                            } else if inner.session_id != sid && inner.status != TaskStatus::Failed
                            {
                                // Provider emitted a session_id that doesn't
                                // match the one we asked to resume. Mark failed
                                // so the caller doesn't trust a forked session
                                // as a successful continuation.
                                let requested = inner.session_id.clone();
                                inner.status = TaskStatus::Failed;
                                inner.stderr.push_str(&format!(
                                    "\nsession fork detected: requested resume of {requested}, provider emitted {sid}"
                                ));
                            }
                        }
                        inner.last_assistant_message.as_ref().map(|msg| {
                            const TAIL_CHARS: usize = 160;
                            let count = msg.chars().count();
                            if count > TAIL_CHARS {
                                let skip = count - TAIL_CHARS;
                                let tail: String = msg.chars().skip(skip).collect();
                                format!("…{tail}")
                            } else {
                                msg.clone()
                            }
                        })
                    };

                    if let Some(snippet) = snippet_to_emit {
                        if last_emitted_snippet.as_deref() != Some(snippet.as_str()) {
                            let _ = tail_tx_clone.send(tail::TailEvent::TaskProgress {
                                task_id: task_id_clone.clone(),
                                activity: snippet.clone(),
                            });
                            last_emitted_snippet = Some(snippet);
                        }
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });
    } else {
        // Bulk stdout collection
        let task_ref_bulk = task.clone();
        tokio::spawn(async move {
            let mut buf = String::new();
            let mut reader = tokio::io::BufReader::new(stdout);
            loop {
                let mut chunk = String::new();
                match reader.read_line(&mut chunk).await {
                    Ok(0) => break,
                    Ok(_) => buf.push_str(&chunk),
                    Err(_) => break,
                }
            }
            if !buf.trim().is_empty() {
                let mut inner = task_ref_bulk.inner.lock();
                let mut sink = EventSink {
                    last_assistant_message: inner.last_assistant_message.clone(),
                    usage: inner.usage.clone(),
                    cost_usd: inner.cost_usd,
                    num_turns: inner.num_turns,
                    session_id: None,
                };
                provider.parse_bulk_output(buf.trim(), &mut sink);
                inner.last_assistant_message = sink.last_assistant_message;
                inner.usage = sink.usage;
                inner.cost_usd = sink.cost_usd;
                inner.num_turns = sink.num_turns;
                if let Some(sid) = sink.session_id {
                    if inner.session_id == "pending" {
                        inner.session_id = sid;
                    } else if inner.session_id != sid && inner.status != TaskStatus::Failed {
                        let requested = inner.session_id.clone();
                        inner.status = TaskStatus::Failed;
                        inner.stderr.push_str(&format!(
                            "\nsession fork detected: requested resume of {requested}, provider emitted {sid}"
                        ));
                    }
                }
            }
            let _ = stdout_done_tx.send(());
        });
    }

    // Spawn stderr reader
    let task_ref_err = task.clone();
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr_handle);
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut inner = task_ref_err.inner.lock();
            inner.stderr.push_str(&line);
            inner.stderr.push('\n');
        }
    });

    // Spawn process waiter — waits for BOTH the process exit AND stdout reader
    // to finish before marking the task terminal. This ensures results are
    // fully parsed before waiters are notified.
    let task_ref_wait = task.clone();
    let task_id_wait = id.clone();
    let tail_tx_wait = tail_tx;
    tokio::spawn(async move {
        let status = child.wait().await;
        // Wait for stdout reader to finish before processing results —
        // ensures all events/results are parsed before we mark terminal.
        let _ = stdout_done_rx.await;
        let code = status.ok().and_then(|s| s.code());

        // Post-hoc vibe session discovery
        if provider == Provider::Vibe {
            let inner = task_ref_wait.inner.lock();
            if inner.session_id == "pending" {
                if let Some(ref c) = inner.cwd {
                    let start = inner.started_at;
                    let cwd_clone = c.clone();
                    drop(inner); // release lock before blocking call
                    if let Some(sid) = providers::discover_vibe_session(start, &cwd_clone) {
                        task_ref_wait.inner.lock().session_id = sid;
                    }
                }
            }
        }

        {
            let mut inner = task_ref_wait.inner.lock();
            inner.exit_code = code;
            // Preserve terminal states set during stream parsing (Cancelled
            // on kill, Failed on session fork detection) — don't let a
            // clean exit code flip a detected failure back to Completed.
            if inner.status != TaskStatus::Cancelled && inner.status != TaskStatus::Failed {
                inner.status = if code == Some(0) {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Failed
                };
            }
            inner.completed_at = Some(now_ms());

            let elapsed = format_elapsed(inner.started_at, inner.completed_at);
            match inner.status {
                TaskStatus::Completed => {
                    let _ = tail_tx_wait.send(tail::TailEvent::TaskCompleted {
                        task_id: task_id_wait.clone(),
                        elapsed,
                        cost: inner.cost_usd,
                    });
                }
                TaskStatus::Failed => {
                    let _ = tail_tx_wait.send(tail::TailEvent::TaskFailed {
                        task_id: task_id_wait.clone(),
                        elapsed,
                        error: inner.stderr.chars().take(200).collect(),
                    });
                }
                _ => {}
            }
        }

        // Propagate session ID to team members
        {
            let inner = task_ref_wait.inner.lock();
            if inner.session_id != "pending" {
                let sid = inner.session_id.clone();
                let tid = inner.id.clone();
                drop(inner);
                team::propagate_session_id(&tid, &sid, &store_dir);
            }
        }

        // Persist and notify waiters
        task_store.read().persist(&store_dir);
        task_ref_wait.notify.notify_waiters();
    });

    task
}

/// Wait for a task to complete. Returns immediately if already terminal.
/// Uses `enable()` on the Notify future before checking status to avoid
/// lost-wakeup races (TOCTOU between status check and await).
pub async fn wait_for_task(task: &Task) {
    loop {
        // Register interest BEFORE checking status — avoids lost wakeup if
        // the task completes between our check and our await.
        let notified = task.notify.notified();
        tokio::pin!(notified);
        // Enable the future so it will capture a notify even if we haven't
        // .await'd yet (this is the critical fix for the race).
        notified.as_mut().enable();

        {
            let inner = task.inner.lock();
            if inner.status.is_terminal() {
                return;
            }
        }
        notified.await;
    }
}

/// Wait with timeout. Returns true if completed, false if timed out.
pub async fn wait_for_task_with_timeout(task: &Task, timeout_secs: Option<f64>) -> bool {
    match timeout_secs {
        None => {
            wait_for_task(task).await;
            true
        }
        Some(secs) => {
            let duration = std::time::Duration::from_secs_f64(secs);
            match tokio::time::timeout(duration, wait_for_task(task)).await {
                Ok(()) => true,
                Err(_) => false,
            }
        }
    }
}

/// Cancel a running task.
pub fn cancel_task(
    task: &Task,
    task_store: &RwLock<TaskStore>,
    store_dir: &std::path::Path,
) -> Result<(), String> {
    let mut inner = task.inner.lock();
    if inner.status != TaskStatus::Running {
        return Err(format!(
            "Task already {}",
            serde_json::to_string(&inner.status).unwrap_or_default()
        ));
    }
    inner.status = TaskStatus::Cancelled;
    inner.completed_at = Some(now_ms());
    drop(inner);

    // Kill the child process
    if let Some(pid) = task.child_id.lock().take() {
        unsafe {
            libc::kill(pid as libc::pid_t, libc::SIGTERM);
        }
    }
    task_store.read().persist(store_dir);
    task.notify.notify_waiters();
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

pub fn format_elapsed(started_at: u64, completed_at: Option<u64>) -> String {
    let end = completed_at.unwrap_or_else(now_ms);
    let ms = end.saturating_sub(started_at);
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m {}s", s / 60, s % 60)
    }
}

pub fn task_result_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let mut obj = serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "elapsed": format_elapsed(inner.started_at, inner.completed_at),
    });

    if let Some(ref msg) = inner.last_assistant_message {
        obj["result"] = Value::String(msg.clone());
    }
    if inner.status == TaskStatus::Completed || inner.status == TaskStatus::Failed {
        if let Some(ref u) = inner.usage {
            obj["usage"] = serde_json::json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
            });
        }
        if let Some(cost) = inner.cost_usd {
            obj["costUsd"] = Value::from(cost);
        }
        if let Some(turns) = inner.num_turns {
            obj["numTurns"] = Value::from(turns);
        }
    }
    if inner.status == TaskStatus::Failed {
        if let Some(code) = inner.exit_code {
            obj["exitCode"] = Value::from(code);
        }
        if !inner.stderr.is_empty() {
            let truncated: String = inner.stderr.chars().take(2000).collect();
            obj["stderr"] = Value::String(truncated);
        }
    }
    obj
}

pub fn task_status_json(task: &Task, tail: usize) -> Value {
    let mut obj = task_result_json(task);
    let inner = task.inner.lock();
    obj["eventCount"] = Value::from(inner.events.len());
    if tail > 0 && !inner.events.is_empty() {
        let start = inner.events.len().saturating_sub(tail);
        obj["recentEvents"] = Value::Array(inner.events[start..].to_vec());
    }
    obj
}

pub fn timeout_snapshot_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let elapsed = format_elapsed(inner.started_at, None);
    let event_count = inner.events.len();
    let last_activity = inner.last_assistant_message.as_deref().map(|msg| {
        let clean = msg.replace('\n', " ");
        if clean.len() > 80 {
            format!("{}…", &clean[..80])
        } else {
            clean
        }
    });

    let keep_going = if inner.status.is_terminal() {
        "no"
    } else if event_count > 0 {
        "yes"
    } else {
        "check_status"
    };

    serde_json::json!({
        "taskId": inner.id,
        "provider": inner.provider,
        "sessionId": inner.session_id,
        "status": inner.status,
        "timed_out": true,
        "elapsed": elapsed,
        "eventCount": event_count,
        "keep_going": keep_going,
        "lastAssistantSnippet": last_activity,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_format_elapsed() {
        assert_eq!(format_elapsed(1000, Some(6000)), "5s");
        assert_eq!(format_elapsed(1000, Some(91000)), "1m 30s");
    }

    #[test]
    fn ambient_emits_scope_block_no_text_guard() {
        let ctx = AmbientContext {
            session_id: Some("sess-abc".into()),
            project_dir: Some("/repo/x".into()),
            bro_name: Some("executor".into()),
            allow_recursion: false,
            provider: Some(providers::Provider::Claude),
            ..Default::default()
        };
        let out = apply_ambient("do stuff", &ctx);
        assert!(!out.contains("IMPORTANT:"), "text recursion guard retired");
        assert!(out.contains("[scope]"));
        assert!(out.contains("session: sess-abc"));
        assert!(out.contains("project: /repo/x"));
        assert!(out.contains("bro: executor"));
        assert!(out.contains("do stuff"));
        assert!(!out.contains("STRUCTURED SIDE CHANNEL"));
    }

    #[test]
    fn ambient_no_text_guard_for_any_provider() {
        // Every provider relies on mechanical filtering now. Vibe has
        // no MCP to recurse through at all.
        for p in [
            providers::Provider::Claude,
            providers::Provider::Copilot,
            providers::Provider::Codex,
            providers::Provider::Gemini,
            providers::Provider::Vibe,
        ] {
            let ctx = AmbientContext {
                allow_recursion: false,
                provider: Some(p),
                ..Default::default()
            };
            let out = apply_ambient("work", &ctx);
            assert!(
                !out.contains("IMPORTANT:"),
                "text guard leaked for provider {p:?}"
            );
        }
    }

    #[test]
    fn ambient_skips_pending_session() {
        let ctx = AmbientContext {
            session_id: Some("pending".into()),
            project_dir: Some("/repo/x".into()),
            ..Default::default()
        };
        let out = apply_ambient("x", &ctx);
        assert!(
            !out.contains("session:"),
            "pending session should be elided"
        );
        assert!(out.contains("project: /repo/x"));
    }

    #[test]
    fn ambient_allow_recursion_skips_everything() {
        let ctx = AmbientContext {
            allow_recursion: true,
            ..Default::default()
        };
        assert_eq!(apply_ambient("raw", &ctx), "raw");
    }

    #[test]
    fn ambient_emits_completion_contract_when_present() {
        let ctx = AmbientContext {
            completion_contract: Some(
                "call bbox_note(kind=\"done\", body=\"summary\") before returning".into(),
            ),
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[completion contract]"));
        assert!(out.contains("bbox_note"));
    }

    #[test]
    fn ambient_emits_recall_directive() {
        let ctx = AmbientContext::default();
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[recall before acting]"));
        assert!(out.contains("bbox_knowledge"));
        assert!(out.contains("FIRST tool call"));
    }

    #[test]
    fn ambient_recall_directive_skipped_under_allow_recursion() {
        let ctx = AmbientContext {
            allow_recursion: true,
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(!out.contains("[recall before acting]"));
        assert!(!out.contains("bbox_knowledge"));
        assert_eq!(out, "work");
    }

    #[test]
    fn brofile_lens_prepends_persona() {
        assert_eq!(
            apply_brofile_lens("work", Some("You are a reviewer")),
            "You are a reviewer\n\nwork"
        );
        assert_eq!(apply_brofile_lens("work", None), "work");
        assert_eq!(apply_brofile_lens("work", Some("   ")), "work");
    }

    #[test]
    fn ambient_and_lens_compose_cleanly() {
        let ctx = AmbientContext {
            session_id: Some("sess-xyz".into()),
            allow_recursion: false,
            provider: Some(providers::Provider::Claude),
            ..Default::default()
        };
        let wrapped = apply_brofile_lens(&apply_ambient("work", &ctx), Some("You are a reviewer"));
        assert!(wrapped.starts_with("You are a reviewer"));
        assert!(wrapped.contains("[scope]"));
        assert!(wrapped.contains("sess-xyz"));
        assert!(wrapped.contains("work"));
        assert!(!wrapped.contains("IMPORTANT:"), "text guard retired");
    }

    #[test]
    fn test_task_result_json_completed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: Some("Done!".into()),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                }),
                cost_usd: Some(0.05),
                num_turns: Some(3),
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(5000),
                exit_code: Some(0),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["taskId"], "t1");
        assert_eq!(json["result"], "Done!");
        assert_eq!(json["costUsd"], 0.05);
        assert_eq!(json["usage"]["input_tokens"], 100);
    }

    #[test]
    fn test_task_result_json_failed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Codex,
                session_id: "s2".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: "something went wrong".into(),
                status: TaskStatus::Failed,
                started_at: 1000,
                completed_at: Some(2000),
                exit_code: Some(1),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["exitCode"], 1);
        assert!(
            json["stderr"]
                .as_str()
                .unwrap()
                .contains("something went wrong")
        );
    }
}

#[cfg(test)]
mod async_tests {
    use super::*;
    use std::sync::Arc;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn test_wait_for_task_already_terminal() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t1".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(0),
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        // Should return immediately without blocking
        wait_for_task(&task).await;
    }

    #[tokio::test]
    async fn test_wait_for_task_notify_race() {
        // Simulate the race: task completes between status check and await
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let task_clone = task.clone();
        // Complete the task after a brief delay
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            {
                let mut inner = task_clone.inner.lock();
                inner.status = TaskStatus::Completed;
                inner.completed_at = Some(now_ms());
            }
            task_clone.notify.notify_waiters();
        });

        // This should not hang even if the notify fires during the gap
        let completed = wait_for_task_with_timeout(&task, Some(5.0)).await;
        assert!(completed, "wait_for_task should have completed");
    }

    #[tokio::test]
    async fn test_wait_for_task_timeout() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t3".into(),
                provider: Provider::Claude,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Running,
                started_at: now_ms(),
                completed_at: None,
                exit_code: None,
                cwd: None,
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        // Should timeout after 0.1s
        let completed = wait_for_task_with_timeout(&task, Some(0.1)).await;
        assert!(!completed, "should have timed out");
    }
}

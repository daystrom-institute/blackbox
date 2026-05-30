//! `FleetOrchestrator` — the daemon-free façade the `bro fleet` cockpit drives.
//!
//! The cockpit links the `blackbox` lib and spawns top-level entrypoint agents
//! **in-process** — no HTTP to a running `blackboxd` (design
//! `design/orchestration/fleet-tui.md` §3). This façade owns the three plain
//! values `spawn_task` needs — a `TaskStore`, a tail `broadcast::Sender`, and a
//! `store_dir` — and hands the cockpit a single `dispatch` entry point plus a
//! tail subscription. Ownership is clean: the cockpit owns exactly the tasks it
//! spawned (it keeps the returned `Arc<Task>` handles), so the façade stays
//! intentionally thin.
//!
//! Net-new item 7 in the design's "what needs to be added" list. The keystone
//! bidirectional control protocol (persistent stdin, `control_request`,
//! `/compact`) is **not** here — that is harness + dispatch-seam work tracked
//! separately. v1 dispatch reuses the existing one-shot `build_exec_args` /
//! `spawn_task` path so the cockpit shell is buildable and runnable today; live
//! steering lands once the bidirectional seam exists.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

use super::providers::ExecOpts;
use super::{Task, TaskStore, spawn_task, spawn_task_interactive};

// Re-export the consumer-facing types so the `bro fleet` cockpit depends only
// on `blackbox::fleet::*` and never reaches into the crate-private
// `orchestration` module directly. `Task` itself is NOT re-exported — the
// cockpit handles agents through the opaque [`AgentHandle`] and reads state via
// [`TaskSnapshot`], so the crate-private `TaskInner` (and its private-typed
// fields) never leak into the public API.
pub use super::providers::Provider;
pub use super::tail::TailEvent;
pub use super::TaskStatus;

/// What to dispatch as a new top-level entrypoint agent. The cockpit's
/// composer fills this in; cwd/model are optional and resolved per dispatch
/// (no stickiness on the agent itself — provider is fixed at spawn, §4).
#[derive(Debug, Clone)]
pub struct DispatchSpec {
    pub provider: Provider,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Extra env overrides for the child (e.g. MCP injection wiring). The
    /// cockpit's TUI-local config (§5.2) feeds this; `None` for a bare launch.
    pub env_overrides: Option<HashMap<String, String>>,
    /// Display name persisted with the task (stored in `bro_label`) so it
    /// survives a cockpit reload. Defaults to the initial prompt's head.
    pub name: Option<String>,
}

impl DispatchSpec {
    pub fn new(provider: Provider, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            prompt: prompt.into(),
            cwd: None,
            model: None,
            env_overrides: None,
            name: None,
        }
    }
}

/// Resume a prior (Interrupted / reloaded) session and continue it with a new
/// turn — `--resume <session_id> -p <prompt>` (§5: steering an Interrupted
/// session auto-resumes). The harness/Claude CLI reloads the on-disk transcript.
#[derive(Debug, Clone)]
pub struct ResumeSpec {
    pub provider: Provider,
    pub session_id: String,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub name: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
}

impl ResumeSpec {
    pub fn new(provider: Provider, session_id: impl Into<String>, prompt: impl Into<String>) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            prompt: prompt.into(),
            cwd: None,
            model: None,
            name: None,
            env_overrides: None,
        }
    }
}

/// Opaque handle to a dispatched entrypoint agent. Wraps the live task; the
/// cockpit holds these (it owns exactly what it spawned) and reads state
/// through [`AgentHandle::snapshot`] without touching crate-private internals.
///
/// For bidi-capable providers the handle also holds the child's writable stdin
/// (behind an async mutex so `&self` steer calls serialize), the channel for
/// driving the persistent session per fleet-tui.md §1.
#[derive(Clone)]
pub struct AgentHandle {
    task: Arc<Task>,
    stdin: Option<Arc<AsyncMutex<tokio::process::ChildStdin>>>,
}

impl AgentHandle {
    pub fn id(&self) -> String {
        self.task.id()
    }

    /// True when this agent runs a persistent bidirectional session and can be
    /// steered (user-turns / control_requests). False for one-shot providers
    /// (Codex et al., §2.1) — steering those is unsupported.
    pub fn can_steer(&self) -> bool {
        self.stdin.is_some()
    }

    /// Write one NDJSON control-plane line to the session's stdin.
    async fn write_line(&self, line: String) -> anyhow::Result<()> {
        let Some(stdin) = &self.stdin else {
            anyhow::bail!("agent is not an interactive session — cannot steer");
        };
        let mut guard = stdin.lock().await;
        guard.write_all(line.as_bytes()).await?;
        guard.write_all(b"\n").await?;
        guard.flush().await?;
        Ok(())
    }

    /// Send a user-turn message (a steer / reply) into the live session (§1.1).
    /// Queues at the agent's next turn boundary if a turn is in flight.
    pub async fn send_user_turn(&self, text: &str) -> anyhow::Result<()> {
        self.write_line(user_turn_ndjson(text)).await
    }

    /// `control_request{interrupt}` — cancel the running turn (§1.1, `Esc`).
    pub async fn interrupt(&self) -> anyhow::Result<()> {
        self.write_line(control_ndjson("interrupt", serde_json::Map::new()))
            .await
    }

    /// `control_request{set_model}` — switch the model for subsequent turns.
    pub async fn set_model(&self, model: &str) -> anyhow::Result<()> {
        let mut extra = serde_json::Map::new();
        extra.insert("model".into(), Value::String(model.to_string()));
        self.write_line(control_ndjson("set_model", extra)).await
    }

    /// `/compact` — an in-stream slash command delivered as a user turn; the
    /// agent emits a `compact_boundary` in response (§1.1, §2.4).
    pub async fn compact(&self) -> anyhow::Result<()> {
        self.send_user_turn("/compact").await
    }

    /// Point-in-time copy of the agent's live state, read under one lock.
    pub fn snapshot(&self) -> TaskSnapshot {
        let inner = self.task.inner.lock();
        // Walk the raw stream-json buffer for harness-envelope state the daemon
        // parser doesn't surface: turn-in-flight (between `result` boundaries)
        // and the builtin `report` tool's needs-input signal (§2.2). The daemon
        // parser ignores `report` lines, so the cockpit derives them here.
        let stream = derive_stream_state(&inner.events);
        TaskSnapshot {
            status: inner.status,
            provider: inner.provider,
            session_id: inner.session_id.clone(),
            last_assistant_message: inner.last_assistant_message.clone(),
            // Prefer the harness `report` line; fall back to the daemon BroReport
            // (populated only for bro_exec-style tasks, never fleet agents).
            report_message: stream
                .report_message
                .or_else(|| inner.report.as_ref().map(|r| r.message.clone())),
            needs_input: stream.needs_input,
            turn_active: stream.turn_active,
            cost_usd: inner.cost_usd,
            num_turns: inner.num_turns,
            started_at: inner.started_at,
            // Wall-clock of the last observed stream event — "last interaction",
            // a roster timing column + sort axis. Stamped by the reader on every
            // event via supervision.observe_event.
            last_event_at_ms: inner.supervision.last_event_at_ms,
            cwd: inner.cwd.clone(),
            stderr: inner.stderr.clone(),
            model: model_from_events(&inner.events),
            // The cockpit's durable display name (stored in bro_label, §5).
            name: inner.bro_label.clone(),
        }
    }

    /// The full verbose inline transcript, parsed fresh from the live event
    /// buffer (§5.4). The cockpit renders this in the detail / single-agent
    /// view; called only for the focused agent, not per roster row.
    pub fn transcript(&self) -> Vec<TranscriptItem> {
        parse_transcript(&self.task.inner.lock().events)
    }
}

/// One rendered item in the verbose inline transcript (§5.4). The fleet layer
/// owns this model rather than reusing `transcripts/types.rs` so the live
/// cockpit view stays decoupled from the stored-transcript schema.
#[derive(Debug, Clone, PartialEq)]
pub enum TranscriptItem {
    /// An operator steer / reply (`▌ you ›`).
    UserSteer(String),
    /// Assistant prose (rendered as markdown).
    AssistantText(String),
    /// Extended-thinking block (`✻`, dim).
    Thinking(String),
    /// A tool call with its raw JSON arguments (`⏺ name`).
    ToolCall { name: String, args: String },
    /// A tool result. `tool` is the originating tool name (correlated by
    /// tool_use_id) so the renderer can show change-making tools (Edit/MCP)
    /// verbosely while suppressing noisy output (Bash). `is_error` renders red.
    ToolResult {
        tool: Option<String>,
        content: String,
        is_error: bool,
    },
    /// The builtin `report` tool's status line (`◆`, §2.2).
    Report { message: String, needs_input: bool },
    /// A `/compact` or auto-compaction boundary divider (§2.4).
    CompactBoundary { trigger: String },
    /// End-of-turn footer with usage/cost.
    TurnFooter {
        num_turns: Option<u64>,
        cost_usd: Option<f64>,
    },
}

/// Parse the raw stream-json buffer into ordered transcript items (§5.4 — the
/// net-new live-stream parser). Handles the harness/Claude envelope: assistant
/// content blocks (text / thinking / tool_use), user events (steers and
/// tool_result echoes), `report`, `compact_boundary`, and per-turn `result`
/// footers. `stream_event` partial deltas are skipped — the full `assistant`
/// event at the turn boundary supersedes them (token-streaming is a refinement).
pub fn parse_transcript(events: &[Value]) -> Vec<TranscriptItem> {
    let mut out = Vec::new();
    // tool_use_id → tool name, so tool_result echoes can be attributed to the
    // tool that produced them (drives verbose-vs-quiet rendering).
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for e in events {
        match e.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "user" => parse_user_event(e, &mut out, &tool_names),
            "assistant" => parse_assistant_event(e, &mut out, &mut tool_names),
            "report" => {
                let r = &e["report"];
                if let Some(msg) = r.get("message").and_then(|m| m.as_str()) {
                    out.push(TranscriptItem::Report {
                        message: msg.to_string(),
                        needs_input: r
                            .get("needs_input")
                            .and_then(|n| n.as_bool())
                            .unwrap_or(false),
                    });
                }
            }
            "system" if e.get("subtype").and_then(|s| s.as_str()) == Some("compact_boundary") => {
                let trigger = e
                    .get("compact_metadata")
                    .and_then(|m| m.get("trigger"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("manual")
                    .to_string();
                out.push(TranscriptItem::CompactBoundary { trigger });
            }
            "result" => out.push(TranscriptItem::TurnFooter {
                num_turns: e.get("num_turns").and_then(|n| n.as_u64()),
                cost_usd: e.get("total_cost_usd").and_then(|c| c.as_f64()),
            }),
            _ => {}
        }
    }
    out
}

/// A `user` event is either an operator steer (string / text-block content) or
/// a tool_result echo (tool_result blocks).
fn parse_user_event(e: &Value, out: &mut Vec<TranscriptItem>, tool_names: &HashMap<String, String>) {
    match &e["message"]["content"] {
        Value::String(s) if !s.trim().is_empty() => {
            out.push(TranscriptItem::UserSteer(s.clone()))
        }
        Value::Array(blocks) => {
            let mut steer = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => out.push(TranscriptItem::ToolResult {
                        tool: b
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .and_then(|id| tool_names.get(id).cloned()),
                        content: extract_content_text(b.get("content")),
                        is_error: b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                    }),
                    Some("text") | Some("input_text") => {
                        if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                            if !steer.is_empty() {
                                steer.push('\n');
                            }
                            steer.push_str(t);
                        }
                    }
                    _ => {}
                }
            }
            if !steer.trim().is_empty() {
                out.push(TranscriptItem::UserSteer(steer));
            }
        }
        _ => {}
    }
}

/// An `assistant` event carries text / thinking / tool_use content blocks.
fn parse_assistant_event(
    e: &Value,
    out: &mut Vec<TranscriptItem>,
    tool_names: &mut HashMap<String, String>,
) {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return;
    };
    for b in blocks {
        match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    if !t.trim().is_empty() {
                        out.push(TranscriptItem::AssistantText(t.to_string()));
                    }
                }
            }
            Some("thinking") => {
                if let Some(t) = b
                    .get("thinking")
                    .and_then(|t| t.as_str())
                    .or_else(|| b.get("text").and_then(|t| t.as_str()))
                {
                    out.push(TranscriptItem::Thinking(t.to_string()));
                }
            }
            Some("tool_use") => {
                let name = b
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("tool")
                    .to_string();
                // Record id → name so the tool_result echo can be attributed.
                if let Some(id) = b.get("id").and_then(|i| i.as_str()) {
                    tool_names.insert(id.to_string(), name.clone());
                }
                out.push(TranscriptItem::ToolCall {
                    name,
                    args: b
                        .get("input")
                        .map(|i| serde_json::to_string_pretty(i).unwrap_or_default())
                        .unwrap_or_default(),
                });
            }
            _ => {}
        }
    }
}

/// tool_result `content` may be a bare string or an array of text blocks.
fn extract_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Read-only snapshot of a dispatched agent's live state — the cockpit's window
/// into a task without naming the crate-private `Task`/`TaskInner`.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub status: TaskStatus,
    pub provider: Provider,
    pub session_id: String,
    pub last_assistant_message: Option<String>,
    /// Latest status line from the harness `report` tool (or daemon BroReport).
    pub report_message: Option<String>,
    /// The latest `report` flagged needs-input — drives the Waiting bucket (§2.2).
    pub needs_input: bool,
    /// A turn is in flight (events streaming past the last `result` boundary) —
    /// distinguishes Active from Idle while the process stays Running (§5).
    pub turn_active: bool,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    /// Wall-clock (ms) the session started.
    pub started_at: u64,
    /// Wall-clock (ms) of the last observed stream event ("last interaction").
    pub last_event_at_ms: Option<u64>,
    pub cwd: Option<String>,
    pub stderr: String,
    pub model: Option<String>,
    /// Durable display name (from `bro_label`) — survives a cockpit reload.
    pub name: Option<String>,
}

/// Harness-envelope state derived from the raw stream-json buffer.
struct StreamState {
    turn_active: bool,
    needs_input: bool,
    report_message: Option<String>,
}

/// Walk the stream-json events chronologically to recover state the daemon
/// parser doesn't track: whether a turn is in flight (toggled by `result`
/// boundaries) and the latest builtin `report` signal (§2.2). `report` lines
/// ride the harness stdout but the daemon's claude parser ignores them, so the
/// cockpit reads them here.
fn derive_stream_state(events: &[serde_json::Value]) -> StreamState {
    let mut turn_active = false;
    let mut needs_input = false;
    let mut report_message = None;
    for e in events {
        match e.get("type").and_then(|t| t.as_str()) {
            // A `result` closes the current turn → Idle until the next turn.
            Some("result") => turn_active = false,
            // Assistant output / tool traffic means a turn is running. (`user`
            // covers both a steer and a tool_result echo; either implies the
            // model is mid-turn.)
            Some("assistant") | Some("stream_event") | Some("user") => turn_active = true,
            // The builtin report tool's status/needs signal.
            Some("report") => {
                let r = &e["report"];
                report_message = r
                    .get("message")
                    .and_then(|m| m.as_str())
                    .map(|s| s.to_string());
                needs_input = r
                    .get("needs_input")
                    .and_then(|n| n.as_bool())
                    .unwrap_or(false);
            }
            _ => {}
        }
    }
    StreamState {
        turn_active,
        needs_input,
        report_message,
    }
}

/// Best-effort model id from an `init`/assistant event in the stream-json buffer.
fn model_from_events(events: &[serde_json::Value]) -> Option<String> {
    events.iter().find_map(|e| {
        e.get("model")
            .or_else(|| e.get("message").and_then(|m| m.get("model")))
            .and_then(|m| m.as_str())
            .map(|s| s.to_string())
    })
}

/// Daemon-free orchestration core for the fleet cockpit. Holds the `TaskStore`,
/// the tail broadcast channel, and the on-disk `store_dir` that `spawn_task`
/// persists task state into.
pub struct FleetOrchestrator {
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf,
}

impl FleetOrchestrator {
    /// Construct over an explicit `store_dir`. Tests and embedders use this;
    /// the cockpit normally goes through [`FleetOrchestrator::from_config`].
    pub fn new(store_dir: PathBuf) -> Self {
        Self::with_store(store_dir, TaskStore::new())
    }

    fn with_store(store_dir: PathBuf, store: TaskStore) -> Self {
        let (tail_tx, _rx) = broadcast::channel(1024);
        Self {
            task_store: Arc::new(RwLock::new(store)),
            tail_tx,
            store_dir,
        }
    }

    /// Build from the resolved blackbox config. The cockpit owns a **dedicated**
    /// store dir (`bro_home/fleet`), isolated from the daemon's own task store,
    /// and **loads** any prior fleet sessions from it — crashed/orphaned
    /// `Running` tasks come back as recoverable (Interrupted, §5). This is why
    /// historical sessions survive a cockpit reload.
    pub fn from_config() -> anyhow::Result<Self> {
        let cfg = crate::config::load()?;
        let store_dir = cfg.paths.bro_home.join("fleet");
        // No age-based eviction: the cockpit's model is manual cleanup (§5), so
        // historical sessions persist until explicitly deleted, not by TTL.
        let store = TaskStore::load(&store_dir, u64::MAX);
        Ok(Self::with_store(store_dir, store))
    }

    /// Flush the current task store to disk so a later cockpit launch can
    /// reload these sessions. The cockpit calls this after each dispatch and on
    /// quit (spawn_task only persists at task-terminal on its own).
    pub fn persist(&self) {
        self.task_store.read().persist(&self.store_dir);
    }

    /// Subscribe to the tail stream. Each call returns an independent receiver;
    /// the cockpit forwards these into its (sync) TUI loop the same way
    /// `council_tui` forwards SSE signals.
    pub fn subscribe(&self) -> broadcast::Receiver<TailEvent> {
        self.tail_tx.subscribe()
    }

    /// Handles to every task this orchestrator has spawned. The cockpit
    /// normally keeps the handles returned by [`dispatch`] directly (it owns
    /// exactly what it spawned), so this is a convenience for
    /// recovery/enumeration paths.
    pub fn tasks(&self) -> Vec<AgentHandle> {
        self.task_store
            .read()
            .all_tasks()
            .into_iter()
            .map(|task| AgentHandle { task, stdin: None })
            .collect()
    }

    pub fn store_dir(&self) -> &std::path::Path {
        &self.store_dir
    }

    /// Spawn a new top-level entrypoint agent. Bidi-capable providers (Claude,
    /// GLM, DeepSeek, Brodex) launch a **persistent bidirectional session**
    /// (`--input-format stream-json --replay-user-messages`, keystone §2) with
    /// stdin kept open for steering; other providers fall back to one-shot
    /// dispatch (no steering, §2.1). Returns an [`AgentHandle`] — the cockpit
    /// holds it to read state and drive the session.
    pub fn dispatch(&self, spec: DispatchSpec) -> AgentHandle {
        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = uuid::Uuid::new_v4().to_string();

        let opts = ExecOpts {
            model: spec.model.clone(),
            effort: None,
            provider_defaults: None,
        };
        let args = spec.provider.build_exec_args(
            &spec.prompt,
            &session_id,
            spec.cwd.as_deref(),
            Some(&opts),
        );

        // The initial `-p <prompt>` from build_exec_args becomes the first user
        // turn; bidi providers add the input-format flags in launch_interactive
        // and ride subsequent turns/controls on the open stdin.
        let bidi = provider_supports_bidi(spec.provider);

        // Resolve transport env (ANTHROPIC_BASE_URL + credentials for the
        // harness providers; selects Anthropic vs Responses transport). Without
        // this the bro-harness child fails at Session::build. Mirrors the daemon
        // dispatch path; user-supplied overrides win on conflict.
        let resolved_env = super::brofile::resolve_provider_env(
            spec.provider,
            None,
            spec.model.as_deref(),
            &self.store_dir,
            None,
        );
        let env_overrides = merge_env(resolved_env, spec.env_overrides);

        // Fleet agents are entrypoint agents, not bros, so `bro_label` carries no
        // team/brofile meaning here — the cockpit reuses it as the durable
        // display name (the initial prompt's head, or an explicit name) so the
        // row survives a reload (§2.2, §5).
        let label = spec
            .name
            .clone()
            .or_else(|| Some(prompt_head(&spec.prompt)));
        if bidi {
            self.launch_interactive(task_id, spec.provider, args, session_id, spec.cwd, label, env_overrides)
        } else {
            let task = spawn_task(
                task_id,
                spec.provider,
                args,
                session_id,
                spec.cwd,
                env_overrides,
                self.store_dir.clone(),
                self.task_store.clone(),
                self.tail_tx.clone(),
                label,
                None,
                None,
            );
            AgentHandle { task, stdin: None }
        }
    }

    /// Resume a prior session (§5). Builds `--resume <id> -p <prompt>` and
    /// relaunches a persistent bidirectional session continuing the same
    /// `session_id`; the prompt is the first turn of the resumed conversation.
    /// Bidi-capable providers only.
    pub fn resume(&self, spec: ResumeSpec) -> AgentHandle {
        let task_id = uuid::Uuid::new_v4().to_string();
        let opts = ExecOpts {
            model: spec.model.clone(),
            effort: None,
            provider_defaults: None,
        };
        let args = spec
            .provider
            .build_resume_args(&spec.session_id, &spec.prompt, Some(&opts));
        let resolved_env = super::brofile::resolve_provider_env(
            spec.provider,
            None,
            spec.model.as_deref(),
            &self.store_dir,
            None,
        );
        let env_overrides = merge_env(resolved_env, spec.env_overrides);
        self.launch_interactive(
            task_id,
            spec.provider,
            args,
            spec.session_id,
            spec.cwd,
            spec.name,
            env_overrides,
        )
    }

    /// Drop a task from the store (used after a resume supersedes the old
    /// Interrupted task, so a reload doesn't show a stale duplicate).
    pub fn forget(&self, task_id: &str) {
        self.task_store
            .write()
            .retain_drop(|t| t.id() != task_id);
        self.persist();
    }

    /// Spawn a persistent bidirectional session from already-built provider args
    /// (shared by dispatch + resume): append the input-format flags, keep stdin
    /// writable, wrap into an [`AgentHandle`].
    fn launch_interactive(
        &self,
        task_id: String,
        provider: Provider,
        mut args: Vec<String>,
        session_id: String,
        cwd: Option<String>,
        name: Option<String>,
        env_overrides: Option<HashMap<String, String>>,
    ) -> AgentHandle {
        args.push("--input-format".into());
        args.push("stream-json".into());
        args.push("--replay-user-messages".into());
        let spawned = spawn_task_interactive(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            self.store_dir.clone(),
            self.task_store.clone(),
            self.tail_tx.clone(),
            name,
            None,
            None,
        );
        AgentHandle {
            task: spawned.task,
            stdin: spawned.stdin.map(|s| Arc::new(AsyncMutex::new(s))),
        }
    }
}

/// First line of the prompt, capped — the durable display name for a session.
fn prompt_head(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    line.chars().take(60).collect()
}

/// Merge resolved transport env (base) with caller-supplied overrides (win on
/// conflict). Returns `None` only when both are empty.
fn merge_env(
    base: Option<HashMap<String, String>>,
    overrides: Option<HashMap<String, String>>,
) -> Option<HashMap<String, String>> {
    match (base, overrides) {
        (None, None) => None,
        (Some(m), None) | (None, Some(m)) => Some(m),
        (Some(mut b), Some(o)) => {
            b.extend(o);
            Some(b)
        }
    }
}

/// Providers that speak the persistent bidirectional stream-json control
/// protocol: Claude (CLI bidi mode) and the bro-harness providers GLM /
/// DeepSeek / Brodex (§2). Others are one-shot only. Public so the cockpit can
/// tell whether a non-live agent is resumable.
pub fn provider_supports_bidi(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Claude | Provider::Glm | Provider::Deepseek | Provider::Brodex
    )
}

/// One NDJSON user-turn message for the harness/Claude input stream
/// (`{"type":"user","message":{"role":"user","content":"…"}}`).
fn user_turn_ndjson(text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "message": { "role": "user", "content": text },
    })
    .to_string()
}

/// One NDJSON `control_request` line with a fresh `request_id`. `extra` carries
/// subtype-specific fields (e.g. `model` for `set_model`).
fn control_ndjson(subtype: &str, extra: serde_json::Map<String, Value>) -> String {
    let mut obj = serde_json::Map::new();
    obj.insert("type".into(), Value::String("control_request".into()));
    obj.insert(
        "request_id".into(),
        Value::String(uuid::Uuid::new_v4().to_string()),
    );
    obj.insert("subtype".into(), Value::String(subtype.into()));
    obj.extend(extra);
    Value::Object(obj).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_orchestrator_has_no_tasks() {
        let orch = FleetOrchestrator::new(std::env::temp_dir().join("bbox-fleet-test"));
        assert!(orch.tasks().is_empty());
        // subscribe must yield a live receiver without a prior dispatch.
        let _rx = orch.subscribe();
    }

    #[test]
    fn build_exec_args_omits_input_format() {
        // launch_interactive owns adding `--input-format`; build_exec_args must
        // NOT, or bidi dispatch doubles the flag and the harness rejects it
        // ("cannot be used multiple times"). Guards that regression.
        for p in [
            Provider::Claude,
            Provider::Glm,
            Provider::Deepseek,
            Provider::Brodex,
        ] {
            let args = p.build_exec_args("hi", "sess", None, None);
            assert!(
                !args.iter().any(|a| a == "--input-format"),
                "{p} build_exec_args must not add --input-format"
            );
        }
    }

    #[test]
    fn bidi_capability_gate() {
        for p in [
            Provider::Claude,
            Provider::Glm,
            Provider::Deepseek,
            Provider::Brodex,
        ] {
            assert!(provider_supports_bidi(p), "{p} should be bidi-capable");
        }
        for p in [Provider::Codex, Provider::Gemini, Provider::Inception] {
            assert!(!provider_supports_bidi(p), "{p} should be one-shot");
        }
    }

    #[test]
    fn user_turn_ndjson_shape() {
        let line = user_turn_ndjson("hi there");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["type"], "user");
        assert_eq!(v["message"]["role"], "user");
        assert_eq!(v["message"]["content"], "hi there");
        assert!(!line.contains('\n'), "NDJSON line must be single-line");
    }

    #[test]
    fn control_ndjson_shape() {
        let interrupt = control_ndjson("interrupt", serde_json::Map::new());
        let v: Value = serde_json::from_str(&interrupt).unwrap();
        assert_eq!(v["type"], "control_request");
        assert_eq!(v["subtype"], "interrupt");
        assert!(v["request_id"].as_str().is_some_and(|s| !s.is_empty()));

        let mut extra = serde_json::Map::new();
        extra.insert("model".into(), Value::String("opus".into()));
        let set_model = control_ndjson("set_model", extra);
        let v: Value = serde_json::from_str(&set_model).unwrap();
        assert_eq!(v["subtype"], "set_model");
        assert_eq!(v["model"], "opus");
    }

    #[test]
    fn stream_state_tracks_turn_and_report() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };

        // Mid-turn: assistant streaming, no result yet.
        let s = derive_stream_state(&[
            ev(r#"{"type":"system","subtype":"init"}"#),
            ev(r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"}]}}"#),
        ]);
        assert!(s.turn_active);
        assert!(!s.needs_input);

        // Turn closed by a result → idle.
        let s = derive_stream_state(&[
            ev(r#"{"type":"assistant","message":{"content":[]}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ]);
        assert!(!s.turn_active);

        // report needs_input while idle → Waiting signal.
        let s = derive_stream_state(&[
            ev(r#"{"type":"report","report":{"message":"blocked on creds","needs_input":true}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ]);
        assert!(!s.turn_active);
        assert!(s.needs_input);
        assert_eq!(s.report_message.as_deref(), Some("blocked on creds"));
    }

    #[test]
    fn transcript_parses_envelope() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"go fix it"}}"#),
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"on it"},
                {"type":"tool_use","id":"t1","name":"shell_run","input":{"cmd":"ls"}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"file.txt","is_error":false}
            ]}}"#),
            ev(r#"{"type":"report","report":{"message":"need a key","needs_input":true}}"#),
            ev(r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}}"#),
            ev(r#"{"type":"result","subtype":"success","num_turns":2,"total_cost_usd":0.01}"#),
        ];
        let items = parse_transcript(&events);
        assert_eq!(items[0], TranscriptItem::UserSteer("go fix it".into()));
        assert_eq!(items[1], TranscriptItem::Thinking("hmm".into()));
        assert_eq!(items[2], TranscriptItem::AssistantText("on it".into()));
        assert!(matches!(&items[3], TranscriptItem::ToolCall { name, .. } if name == "shell_run"));
        assert!(matches!(
            &items[4],
            TranscriptItem::ToolResult { content, is_error: false, tool }
                if content == "file.txt" && tool.as_deref() == Some("shell_run")
        ));
        assert!(matches!(
            &items[5],
            TranscriptItem::Report { needs_input: true, .. }
        ));
        assert!(matches!(
            &items[6],
            TranscriptItem::CompactBoundary { trigger } if trigger == "auto"
        ));
        assert!(matches!(
            items[7],
            TranscriptItem::TurnFooter { num_turns: Some(2), cost_usd: Some(_) }
        ));
    }

    #[test]
    fn dispatch_spec_builder_defaults() {
        let spec = DispatchSpec::new(Provider::Claude, "hello");
        assert_eq!(spec.prompt, "hello");
        assert!(spec.cwd.is_none());
        assert!(spec.model.is_none());
    }
}

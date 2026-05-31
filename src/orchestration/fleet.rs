//! `FleetOrchestrator` — the daemon-free façade the `bro fleet` cockpit drives.
//!
//! The cockpit links the `blackbox` lib and spawns top-level entrypoint agents
//! **in-process** — no HTTP to a running `blackboxd` (design
//! `design/fleet-tui/fleet-tui.md` §3). This façade owns the three plain
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

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, broadcast};

use super::mcp::McpServerConfig;
use super::providers::ExecOpts;
use super::{Task, TaskStore, spawn_task, spawn_task_interactive};

// Re-export the consumer-facing types so the `bro fleet` cockpit depends only
// on `blackbox::fleet::*` and never reaches into the crate-private
// `orchestration` module directly. `Task` itself is NOT re-exported — the
// cockpit handles agents through the opaque [`AgentHandle`] and reads state via
// [`TaskSnapshot`], so the crate-private `TaskInner` (and its private-typed
// fields) never leak into the public API.
pub use super::TaskStatus;
pub use super::providers::Provider;
pub use super::tail::TailEvent;

/// TUI-local fleet config — `fleet.json` beside the selected blackbox
/// `config.toml` but read entirely daemon-free. Deliberately
/// **not** the bbox project registry or the daemon's `mcp.json`: those drag in
/// the daemon plus per-project indexing, inappropriate for the cockpit.
///
/// v1 holds one thing — a normalized MCP server map injected into **every**
/// dispatched agent regardless of provider (`fleet-tui.md` §5.2). The
/// normalization is `McpServerConfig` (the same Http/Sse/Stdio shape the rest of
/// the codebase uses); the only per-provider work is translating it to CLI args
/// at dispatch (`Provider::build_fleet_mcp_args`). Future surfaces (e.g. the
/// `@project` cwd map) hang off this same file.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct FleetConfig {
    #[serde(default, rename = "mcpServers")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,

    /// Extra harness tools to pin into the hot tool surface for every fleet
    /// agent. Fleet also contributes [`DEFAULT_FLEET_PIN_TOOLS`], so this field
    /// is additive rather than replacing the core grounding surface.
    #[serde(
        default,
        rename = "pinTools",
        alias = "pinnedTools",
        alias = "pin_tools"
    )]
    pub pin_tools: Vec<String>,

    /// Experimental classifier-companion ("intern") config. When present, every
    /// executor dispatched from the cockpit gets a paired classifier session
    /// that watches its activity and suggests tools/atoms/skills/strategies. See
    /// [`ClassifierConfig`]. Absent → the feature is off and the cockpit behaves
    /// exactly as before.
    #[serde(default)]
    pub classifier: Option<ClassifierConfig>,
}

/// Config for the experimental classifier companion — the "assistant's
/// assistant". All fields are optional so a bare `{"classifier": {}}` enables it
/// with built-in defaults; the JSON surface is the refinement knob (prompt,
/// model, cadence) the research vehicle is built around.
#[derive(Debug, Clone, Deserialize)]
pub struct ClassifierConfig {
    /// Provider for the classifier session. Must be bidi-capable (it's steered
    /// with executor activity each pass). Lowercase name; default `claude`.
    #[serde(default)]
    pub provider: Option<String>,
    /// Model override for the classifier session (cheap models recommended).
    #[serde(default)]
    pub model: Option<String>,
    /// Effort/thinking level for the classifier session, passed to the CLI's
    /// `--effort` (e.g. "medium"). Threaded through fleet dispatch.
    #[serde(default)]
    pub effort: Option<String>,
    /// System framing + domain knowledge. Empty → [`DEFAULT_CLASSIFIER_PROMPT`].
    #[serde(default)]
    pub prompt: Option<String>,
    /// Seconds between observation passes. Default 4, floored at 1.
    #[serde(default)]
    pub cadence_secs: Option<u64>,
    /// Relay suggestions into the executor as `[INTERN]` turns. Default true.
    /// When false, suggestions are still surfaced in the TUI (observe-only).
    #[serde(default)]
    pub auto_send: Option<bool>,
    /// Minimum new transcript items (tool calls / messages) accrued before the
    /// monitor digests again. This is what lets long turns get periodic mid-turn
    /// check-ins instead of a single end-of-turn look. Default 10.
    #[serde(default)]
    pub min_activity: Option<u32>,
}

impl ClassifierConfig {
    /// Bidi provider the classifier runs on (default Claude). Non-bidi or
    /// unknown names collapse to Claude — the classifier MUST be steerable.
    pub fn provider_resolved(&self) -> Provider {
        match self
            .provider
            .as_deref()
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("glm") => Provider::Glm,
            Some("deepseek") | Some("ds") => Provider::Deepseek,
            Some("brodex") | Some("bdx") => Provider::Brodex,
            _ => Provider::Claude,
        }
    }

    /// The configured prompt, or the calibrated built-in.
    pub fn resolved_prompt(&self) -> String {
        match self.prompt.as_deref() {
            Some(p) if !p.trim().is_empty() => p.to_string(),
            _ => DEFAULT_CLASSIFIER_PROMPT.to_string(),
        }
    }

    pub fn cadence_secs_resolved(&self) -> u64 {
        self.cadence_secs.unwrap_or(4).max(1)
    }

    pub fn auto_send_resolved(&self) -> bool {
        self.auto_send.unwrap_or(true)
    }

    pub fn min_activity_resolved(&self) -> u32 {
        self.min_activity.unwrap_or(10).max(1)
    }
}

/// Prefix on classifier-relayed user turns, disambiguating the intern's voice
/// from the operator's in the executor's transcript. Mirrored in the executor's
/// first-turn rider so the executor knows these turns are advice, not orders.
pub const INTERN_PREFIX: &str = "[INTERN]";

/// Durable-name sentinel for the hidden classifier companion sessions, so the
/// cockpit can keep them out of the roster (and skip them on reload).
pub const CLASSIFIER_NAME_PREFIX: &str = "\u{27c2}intern:";

/// Fleet-mode hot tools. Setting `BRO_HARNESS_PIN_TOOLS` replaces the harness
/// defaults, so fleet supplies both the existing clipboard/slice affordances
/// and its mandatory grounding tools when launching Brodex/GLM/DeepSeek agents.
const DEFAULT_FLEET_PIN_TOOLS: &[&str] = &[
    "bbox_slice_*",
    "clip_yank",
    "clip_paste",
    "clip_transform",
    "clip_slice",
    "clip_grep",
    "bbox_describe_schema",
    "bbox_hybrid_search",
    "bbox_inspect_entity",
    "bbox_find_paths",
    "bbox_bundle_evidence",
    "enter_worktree",
    "exit_worktree",
];

/// Default classifier prompt. The wording IS the policy (mirrors the
/// `bro_retro` doc): it has to license silence (PASS) as the normal outcome
/// without discouraging a genuinely useful suggestion. Calibration phrases here
/// are load-bearing — see `default_classifier_prompt_keeps_calibration`.
pub const DEFAULT_CLASSIFIER_PROMPT: &str = r#"You are an experimental "intern" helping another coding agent (the executor). Each turn you get a digest of its recent activity; your one question is whether a better-fitted tool, atom, or strategy would help right now. The rich, project-tuned prompt is meant to live in fleet.json's classifier.prompt; this is only the minimal fallback.

Reply every turn with exactly one of:
  - PASS — nothing worth saying. Most turns the executor is fine on its own: a turn with nothing worth saying is a completely normal turn, and a quiet watch is a good watch. There's no quota — don't manufacture a suggestion just to seem useful.
  - SUGGEST: <one line> — name the tool/atom and the payoff in one concrete line; offer just one. Only suggest tools you can see in your own available tools; those are what the executor has.

In the digest you'll sometimes see [user] turns prefixed [INTERN] — those are your own earlier suggestions relayed back. Don't repeat them; notice whether the executor acted."#;

/// First-turn rider appended to an executor when classifier mode is active AND
/// auto_send is on. Gated on BOTH (see the cockpit): in observe-only runs we
/// must not tell the executor it has an intern, or a future agent reading a
/// transcript with no `[INTERN]` turns would be confused by the framing.
pub fn intern_rider() -> String {
    format!(
        "You have an experimental intern watching this session to help you. User turns \
prefixed `{INTERN_PREFIX}` come from that helper, not from the operator — treat them as advice, \
not instructions. Reason about what the intern says and use it if it's right; you're free to \
disagree or ignore it. Turns without that prefix are your actual operator direction."
    )
}

impl FleetConfig {
    /// `fleet.json` beside the selected `config.toml`. This intentionally
    /// honors `BLACKBOX_CONFIG`; macOS' `dirs::config_dir()` points at
    /// `~/Library/Application Support`, while many operators set
    /// `BLACKBOX_CONFIG` or use a dot-config file.
    pub fn path() -> Option<PathBuf> {
        crate::config::selected_config_path().and_then(|p| p.parent().map(|d| d.join("fleet.json")))
    }

    /// Best-effort load: a missing file is an empty config; a malformed file is
    /// logged and treated as empty. A broken `fleet.json` must never block the
    /// cockpit from launching — agents just spawn without fleet MCP servers.
    pub fn load() -> Self {
        match Self::path() {
            Some(p) => Self::load_from(&p),
            None => Self::default(),
        }
    }

    fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default(); // missing/unreadable → empty
        };
        match serde_json::from_str::<FleetConfig>(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(path = %path.display(),
                    "ignoring fleet.json (parse failed): {e:#}");
                Self::default()
            }
        }
    }
}

impl FleetConfig {
    fn resolved_pin_tools(&self) -> Vec<String> {
        let mut out = Vec::new();
        for tool in DEFAULT_FLEET_PIN_TOOLS
            .iter()
            .copied()
            .map(str::to_string)
            .chain(self.pin_tools.iter().cloned())
        {
            let tool = tool.trim();
            if !tool.is_empty() && !out.iter().any(|existing| existing == tool) {
                out.push(tool.to_string());
            }
        }
        out
    }
}

/// What to dispatch as a new top-level entrypoint agent. The cockpit's
/// composer fills this in; cwd/model are optional and resolved per dispatch
/// (no stickiness on the agent itself — provider is fixed at spawn, §4).
#[derive(Debug, Clone)]
pub struct DispatchSpec {
    pub provider: Provider,
    pub prompt: String,
    pub cwd: Option<String>,
    pub model: Option<String>,
    /// Effort/thinking level passed to the provider CLI's `--effort`.
    pub effort: Option<String>,
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
            effort: None,
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
    pub effort: Option<String>,
    pub name: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
}

impl ResumeSpec {
    pub fn new(
        provider: Provider,
        session_id: impl Into<String>,
        prompt: impl Into<String>,
    ) -> Self {
        Self {
            provider,
            session_id: session_id.into(),
            prompt: prompt.into(),
            cwd: None,
            model: None,
            effort: None,
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

    /// A clone of this handle with the live stdin dropped — used after the
    /// session is stopped, so the cockpit treats it as non-live (a later steer
    /// resumes it rather than writing to a dead pipe).
    pub fn without_stdin(&self) -> AgentHandle {
        AgentHandle {
            task: self.task.clone(),
            stdin: None,
        }
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
        /// The window-0 diagnostics rider split off the tool body, when the
        /// harness appended one (Rust file edits). Rendered distinctly.
        rider: Option<String>,
    },
    /// The builtin `report` tool's status line (`◆`, §2.2).
    Report { message: String, needs_input: bool },
    /// Current shared TodoWrite state, parsed from the `todo_write` tool result.
    TodoState(TodoState),
    /// A `/compact` or auto-compaction boundary divider (§2.4).
    CompactBoundary { trigger: String },
    /// End-of-turn footer with usage/cost.
    TurnFooter {
        num_turns: Option<u64>,
        cost_usd: Option<f64>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TodoState {
    pub total: usize,
    pub completed: usize,
    pub items: Vec<TodoItem>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TodoItem {
    pub status: TodoItemStatus,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TodoItemStatus {
    Pending,
    InProgress,
    Completed,
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
fn parse_user_event(
    e: &Value,
    out: &mut Vec<TranscriptItem>,
    tool_names: &HashMap<String, String>,
) {
    match &e["message"]["content"] {
        Value::String(s) if !s.trim().is_empty() => out.push(TranscriptItem::UserSteer(s.clone())),
        Value::Array(blocks) => {
            let mut steer = String::new();
            for b in blocks {
                match b.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => {
                        let (content, rider) =
                            split_window0_rider(extract_content_text(b.get("content")));
                        let tool = b
                            .get("tool_use_id")
                            .and_then(|i| i.as_str())
                            .and_then(|id| tool_names.get(id).cloned());
                        if tool.as_deref() == Some("todo_write") {
                            if let Some(todo) = parse_todo_state(&content) {
                                out.push(TranscriptItem::TodoState(todo));
                                continue;
                            }
                        }
                        out.push(TranscriptItem::ToolResult {
                            tool,
                            content,
                            is_error: b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false),
                            rider,
                        })
                    }
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

fn parse_todo_state(content: &str) -> Option<TodoState> {
    let value: Value = serde_json::from_str(content).ok()?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let list = value.get("list").and_then(|v| v.as_str())?;
    let items: Vec<TodoItem> = list.lines().filter_map(parse_todo_item).collect();
    if items.is_empty() {
        return None;
    }
    let total = value
        .get("total")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or(items.len());
    let completed = value
        .get("completed")
        .and_then(|v| v.as_u64())
        .map(|n| n as usize)
        .unwrap_or_else(|| {
            items
                .iter()
                .filter(|i| i.status == TodoItemStatus::Completed)
                .count()
        });
    Some(TodoState {
        total,
        completed,
        items,
    })
}

fn parse_todo_item(line: &str) -> Option<TodoItem> {
    let trimmed = line.trim();
    let (status, text) = if let Some(rest) = trimmed.strip_prefix("[ ]") {
        (TodoItemStatus::Pending, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[~]") {
        (TodoItemStatus::InProgress, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[x]") {
        (TodoItemStatus::Completed, rest)
    } else if let Some(rest) = trimmed.strip_prefix("[X]") {
        (TodoItemStatus::Completed, rest)
    } else {
        return None;
    };
    let text = text.trim();
    (!text.is_empty()).then(|| TodoItem {
        status,
        text: text.to_string(),
    })
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
/// Opening marker of the window-0 diagnostics rider the harness appends to a
/// tool-result body. WIRE CONTRACT: must match `RIDER_MARKER` in
/// `crates/bro-harness/src/diagnostics/render.rs` (the harness crate is a
/// sibling, so the string is duplicated rather than shared).
const WINDOW0_RIDER_MARKER: &str = "window-0 diagnostics:";

/// Split a tool-result body into `(body, rider)` on the window-0 marker. The
/// harness appends the rider after a `\n\n` separator; we surface it separately
/// so the TUI can render diagnostics distinctly from the tool's own output.
fn split_window0_rider(full: String) -> (String, Option<String>) {
    match full.find(WINDOW0_RIDER_MARKER) {
        Some(idx) => (
            full[..idx].trim_end().to_string(),
            Some(full[idx..].trim_end().to_string()),
        ),
        None => (full, None),
    }
}

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
    /// Normalized MCP servers from `fleet.json`, injected into every dispatched
    /// agent via `Provider::build_fleet_mcp_args`. Empty when no config exists.
    mcp_servers: BTreeMap<String, McpServerConfig>,
    /// Experimental classifier-companion config from `fleet.json`; `None` when
    /// the feature is off.
    classifier: Option<ClassifierConfig>,
    /// Resolved fleet-level hot tools injected into Brodex-family harness
    /// children through `BRO_HARNESS_PIN_TOOLS`.
    pin_tools: Vec<String>,
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
            mcp_servers: BTreeMap::new(),
            classifier: None,
            pin_tools: Vec::new(),
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
        let mut orch = Self::with_store(store_dir, store);
        // TUI-local MCP servers injected into every dispatched agent (§5.2),
        // plus the optional classifier-companion config — loaded once.
        let cfg = FleetConfig::load();
        orch.pin_tools = cfg.resolved_pin_tools();
        orch.mcp_servers = cfg.mcp_servers;
        orch.classifier = cfg.classifier;
        Ok(orch)
    }

    /// The classifier-companion config, if `fleet.json` enabled it. The cockpit
    /// reads this at dispatch time to spawn an intern session per executor.
    pub fn classifier(&self) -> Option<&ClassifierConfig> {
        self.classifier.as_ref()
    }

    /// Flush the current task store to disk so a later cockpit launch can
    /// reload these sessions. The cockpit calls this after each dispatch and on
    /// quit (spawn_task only persists at task-terminal on its own).
    pub fn persist(&self) {
        self.task_store.read().persist_all_events(&self.store_dir);
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
            effort: spec.effort.clone(),
            provider_defaults: None,
        };
        let mut args = spec.provider.build_exec_args(
            &spec.prompt,
            &session_id,
            spec.cwd.as_deref(),
            Some(&opts),
        );
        // One fleet-wide MCP set, translated to this provider's CLI flags.
        args.extend(spec.provider.build_fleet_mcp_args(&self.mcp_servers));

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
        let env_overrides = merge_env(
            merge_env(resolved_env, self.pin_tools_env(spec.provider)),
            spec.env_overrides,
        );

        // Fleet agents are entrypoint agents, not bros, so `bro_label` carries no
        // team/brofile meaning here — the cockpit reuses it as the durable
        // display name (the initial prompt's head, or an explicit name) so the
        // row survives a reload (§2.2, §5).
        let label = spec
            .name
            .clone()
            .or_else(|| Some(prompt_head(&spec.prompt)));
        if bidi {
            let seed = bidi_seeds_turn1_via_stdin(spec.provider).then(|| spec.prompt.clone());
            self.launch_interactive(
                task_id,
                spec.provider,
                args,
                session_id,
                spec.cwd,
                label,
                env_overrides,
                seed,
            )
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
            effort: spec.effort.clone(),
            provider_defaults: None,
        };
        let mut args = spec
            .provider
            .build_resume_args(&spec.session_id, &spec.prompt, Some(&opts));
        args.extend(spec.provider.build_fleet_mcp_args(&self.mcp_servers));
        let resolved_env = super::brofile::resolve_provider_env(
            spec.provider,
            None,
            spec.model.as_deref(),
            &self.store_dir,
            None,
        );
        let env_overrides = merge_env(
            merge_env(resolved_env, self.pin_tools_env(spec.provider)),
            spec.env_overrides,
        );
        let seed = bidi_seeds_turn1_via_stdin(spec.provider).then(|| spec.prompt.clone());
        self.launch_interactive(
            task_id,
            spec.provider,
            args,
            spec.session_id,
            spec.cwd,
            spec.name,
            env_overrides,
            seed,
        )
    }

    /// Drop a task from the store (used after a resume supersedes the old
    /// Interrupted task, or on Ctrl+X delete, so a reload doesn't show it). The
    /// underlying provider session jsonl survives on disk regardless (§5).
    pub fn forget(&self, task_id: &str) {
        self.task_store.write().retain_drop(|t| t.id() != task_id);
        self.persist();
    }

    /// Stop a running session (Ctrl+X): SIGTERM the child and mark it Cancelled
    /// (→ Interrupted). The provider session survives on disk for resume.
    pub fn stop(&self, handle: &AgentHandle) -> Result<(), String> {
        super::cancel_task(&handle.task, &self.task_store, &self.store_dir)
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
        seed_prompt: Option<String>,
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
        let handle = AgentHandle {
            task: spawned.task,
            stdin: spawned.stdin.map(|s| Arc::new(AsyncMutex::new(s))),
        };
        // Seed turn-1 over stdin for providers that ignore `-p` in stream-json
        // input mode (Claude). Fire-and-forget on the same runtime that spawned
        // the child's stdout reader; the write queues in the pipe until the CLI
        // reads it. Without this the Claude session launches and hangs (no turn).
        if let Some(seed) = seed_prompt {
            if handle.stdin.is_some() {
                let h = handle.clone();
                tokio::spawn(async move {
                    if let Err(e) = h.send_user_turn(&seed).await {
                        tracing::warn!("fleet: failed to seed first turn over stdin: {e:#}");
                    }
                });
            }
        }
        handle
    }

    fn pin_tools_env(&self, provider: Provider) -> Option<HashMap<String, String>> {
        if self.pin_tools.is_empty() || !provider_uses_bro_harness(provider) {
            return None;
        }
        Some(HashMap::from([(
            "BRO_HARNESS_PIN_TOOLS".to_string(),
            self.pin_tools.join(","),
        )]))
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

/// Whether fleet must write turn-1 to the session's stdin after launch.
///
/// The Claude CLI **ignores `-p`** in `--input-format stream-json` mode — it
/// keeps the session alive and waits for the first user turn on stdin, so a
/// `-p`-only launch just hangs (0 events → Idle). bro-harness is the opposite:
/// its bidi `run_session` seeds turn-1 from `-p` directly
/// (`crates/bro-harness/src/agent_loop.rs`), so writing turn-1 to its stdin too
/// would double the first turn. Hence: seed via stdin for Claude only.
fn bidi_seeds_turn1_via_stdin(provider: Provider) -> bool {
    matches!(provider, Provider::Claude)
}

fn provider_uses_bro_harness(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Glm | Provider::Deepseek | Provider::Brodex
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
    fn fleet_config_parses_normalized_mcp_servers() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{
                "mcpServers": {
                    "context7": { "type": "http", "url": "https://ctx7.example/mcp" },
                    "fs": { "type": "stdio", "command": "fs-mcp", "args": ["--root", "/tmp"] }
                }
            }"#,
        )
        .unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert!(matches!(
            cfg.mcp_servers.get("context7"),
            Some(McpServerConfig::Http { .. })
        ));
        assert!(matches!(
            cfg.mcp_servers.get("fs"),
            Some(McpServerConfig::Stdio { .. })
        ));
    }

    #[test]
    fn fleet_config_missing_mcp_servers_is_empty() {
        // A bare/older fleet.json (e.g. one that only carries a future cwd map)
        // must deserialize, not error — mcpServers defaults to empty.
        let cfg: FleetConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.mcp_servers.is_empty());
    }

    #[test]
    fn fleet_config_pin_tools_are_additive_to_grounding_defaults() {
        let cfg: FleetConfig = serde_json::from_str(r#"{ "pinTools": ["extra_tool"] }"#).unwrap();
        let pins = cfg.resolved_pin_tools();
        assert!(pins.iter().any(|p| p == "bbox_describe_schema"));
        assert!(pins.iter().any(|p| p == "bbox_hybrid_search"));
        assert!(pins.iter().any(|p| p == "extra_tool"));
    }

    #[test]
    fn only_claude_seeds_turn1_via_stdin() {
        // Claude ignores `-p` in stream-json input mode → must seed turn-1 over
        // stdin. bro-harness seeds turn-1 from `-p` → must NOT be stdin-seeded
        // (would double the first turn).
        assert!(bidi_seeds_turn1_via_stdin(Provider::Claude));
        for p in [Provider::Glm, Provider::Deepseek, Provider::Brodex] {
            assert!(
                !bidi_seeds_turn1_via_stdin(p),
                "{p} seeds turn-1 from -p; stdin-seeding would double it"
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
            ev(
                r#"{"type":"user","isReplay":true,"message":{"role":"user","content":"go fix it"}}"#,
            ),
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"thinking","thinking":"hmm"},
                {"type":"text","text":"on it"},
                {"type":"tool_use","id":"t1","name":"shell_run","input":{"cmd":"ls"}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"t1","content":"file.txt","is_error":false}
            ]}}"#),
            ev(r#"{"type":"report","report":{"message":"need a key","needs_input":true}}"#),
            ev(
                r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto"}}"#,
            ),
            ev(r#"{"type":"result","subtype":"success","num_turns":2,"total_cost_usd":0.01}"#),
        ];
        let items = parse_transcript(&events);
        assert_eq!(items[0], TranscriptItem::UserSteer("go fix it".into()));
        assert_eq!(items[1], TranscriptItem::Thinking("hmm".into()));
        assert_eq!(items[2], TranscriptItem::AssistantText("on it".into()));
        assert!(matches!(&items[3], TranscriptItem::ToolCall { name, .. } if name == "shell_run"));
        assert!(matches!(
            &items[4],
            TranscriptItem::ToolResult { content, is_error: false, tool, .. }
                if content == "file.txt" && tool.as_deref() == Some("shell_run")
        ));
        assert!(matches!(
            &items[5],
            TranscriptItem::Report {
                needs_input: true,
                ..
            }
        ));
        assert!(matches!(
            &items[6],
            TranscriptItem::CompactBoundary { trigger } if trigger == "auto"
        ));
        assert!(matches!(
            items[7],
            TranscriptItem::TurnFooter {
                num_turns: Some(2),
                cost_usd: Some(_)
            }
        ));
    }

    #[test]
    fn transcript_parses_todo_write_result_as_todo_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":3,\"completed\":1,\"list\":\"[x] Done\\n[~] Doing\\n[ ] Later\"}","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(&items[0], TranscriptItem::ToolCall { name, .. } if name == "todo_write"));
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 3, completed: 1, items })
                if items.len() == 3
                    && items[0].status == TodoItemStatus::Completed
                    && items[1].status == TodoItemStatus::InProgress
                    && items[2].status == TodoItemStatus::Pending
        ));
    }

    #[test]
    fn dispatch_spec_builder_defaults() {
        let spec = DispatchSpec::new(Provider::Claude, "hello");
        assert_eq!(spec.prompt, "hello");
        assert!(spec.cwd.is_none());
        assert!(spec.model.is_none());
    }

    #[test]
    fn fleet_config_parses_classifier() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{ "classifier": { "provider": "glm", "cadence_secs": 8, "auto_send": false } }"#,
        )
        .unwrap();
        let c = cfg.classifier.expect("classifier present");
        assert!(matches!(c.provider_resolved(), Provider::Glm));
        assert_eq!(c.cadence_secs_resolved(), 8);
        assert!(!c.auto_send_resolved());
        assert_eq!(c.min_activity_resolved(), 10);
        // Empty prompt falls back to the calibrated default.
        assert_eq!(c.resolved_prompt(), DEFAULT_CLASSIFIER_PROMPT);
    }

    #[test]
    fn fleet_config_path_sits_next_to_selected_config() {
        let _guard = crate::util::test_env_lock();
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().canonicalize().unwrap().join("custom.toml");
        unsafe {
            std::env::set_var("BLACKBOX_CONFIG", &config_path);
        }
        assert_eq!(
            FleetConfig::path().as_deref(),
            Some(config_path.with_file_name("fleet.json").as_path())
        );
        unsafe {
            std::env::remove_var("BLACKBOX_CONFIG");
        }
    }

    #[test]
    fn classifier_provider_defaults_to_claude_and_stays_bidi() {
        // Unknown / non-bidi names collapse to Claude — the classifier must be
        // steerable, so it can never resolve to a one-shot provider.
        for name in [None, Some("codex"), Some("gemini"), Some("nonsense")] {
            let c = ClassifierConfig {
                provider: name.map(str::to_string),
                model: None,
                effort: None,
                prompt: None,
                cadence_secs: None,
                auto_send: None,
                min_activity: None,
            };
            assert!(provider_supports_bidi(c.provider_resolved()));
        }
    }

    #[test]
    fn default_classifier_prompt_keeps_calibration() {
        // Mirrors `workload_retro_prompt_keeps_the_no_compulsion_balance`: these
        // phrases are the policy. Losing them re-skews the intern toward nagging
        // (drop the PASS license) or silence (drop the SUGGEST license).
        for phrase in [
            "a completely normal turn",
            "a quiet watch is a good watch",
            "don't manufacture a suggestion",
            "PASS",
            "SUGGEST:",
            "[INTERN]",
        ] {
            assert!(
                DEFAULT_CLASSIFIER_PROMPT.contains(phrase),
                "default classifier prompt lost calibration phrase: {phrase:?}"
            );
        }
    }

    #[test]
    fn intern_rider_frames_advice_not_orders() {
        let r = intern_rider();
        assert!(r.contains(INTERN_PREFIX));
        assert!(r.contains("advice"));
        assert!(r.contains("free to disagree"));
    }
}

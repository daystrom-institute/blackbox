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

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use super::mcp::McpServerConfig;
use super::providers::EventSink;
use super::providers::dispatch_prelude::*;
use super::supervision::SupervisionState;
use super::{Task, TaskInner, TaskStore, format_elapsed, now_ms};

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
/// at dispatch (`Provider::build_fleet_mcp_args`). The `projects` map is the
/// fleet-local `@project` cwd selector (`keyword -> absolute dir`) used by the
/// roster composer.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct FleetConfig {
    #[serde(default, rename = "mcpServers")]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub mcp_servers: BTreeMap<String, McpServerConfig>,

    /// Fleet-local project aliases used by the roster composer:
    /// `@keyword <prompt>` dispatches a new isolated worktree from this cwd.
    /// This is intentionally not the bbox project registry.
    #[serde(default)]
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub projects: BTreeMap<String, String>,

    /// Extra harness tools to pin into the hot tool surface for every fleet
    /// agent. Fleet also contributes [`DEFAULT_FLEET_PIN_TOOLS`], so this field
    /// is additive rather than replacing the core grounding surface.
    #[serde(
        default,
        rename = "pinTools",
        alias = "pinnedTools",
        alias = "pin_tools"
    )]
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub pin_tools: Vec<String>,

    /// Experimental classifier-companion ("intern") config. When enabled, every
    /// executor dispatched from the cockpit gets a paired classifier session
    /// that watches its activity and suggests tools/atoms/skills/strategies. See
    /// [`ClassifierConfig`]. Absent → the feature is off and the cockpit behaves
    /// exactly as before.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classifier: Option<ClassifierConfig>,
}

/// Config for the experimental classifier companion — the "assistant's
/// assistant". All fields are optional so a bare `{"classifier": {}}` enables it
/// with built-in defaults; the JSON surface is the refinement knob (prompt,
/// model, cadence) the research vehicle is built around.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ClassifierConfig {
    /// Explicit enablement for the classifier companion. Presence of a
    /// `classifier` object alone is configuration, not enablement; the config
    /// panel writes this field explicitly.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// Provider for the classifier session. Must be bidi-capable (it's steered
    /// with executor activity each pass). Lowercase name; default `glm`.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model override for the classifier session (cheap models recommended).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Effort/thinking level for the classifier session, passed to the CLI's
    /// `--effort` (e.g. "medium"). Threaded through fleet dispatch.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    /// System framing + domain knowledge. Empty → [`DEFAULT_CLASSIFIER_PROMPT`].
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Seconds between observation passes. Default 4, floored at 1.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cadence_secs: Option<u64>,
    /// Relay suggestions into the executor as `[INTERN]` turns. Default true.
    /// When false, suggestions are still surfaced in the TUI (observe-only).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_send: Option<bool>,
    /// Minimum new transcript items (tool calls / messages) accrued before the
    /// monitor digests again. This is what lets long turns get periodic mid-turn
    /// check-ins instead of a single end-of-turn look. Default 10.
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_activity: Option<u32>,
}

impl ClassifierConfig {
    pub fn enabled_resolved(&self) -> bool {
        self.enabled.unwrap_or(false)
    }

    /// Bidi provider the classifier runs on (default GLM). Non-bidi or unknown
    /// names — including `claude` — collapse to GLM: the classifier MUST be
    /// steerable, and Claude is intentionally not a fleet participant (it can't
    /// execute the `bro-tools` fleet builtins; see `FLEET_PROVIDERS`). GLM is the
    /// default because it's the cheapest steady-state option for the ~4s
    /// observation cadence.
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
            Some("vibebh") | Some("vibe-bh") => Provider::VibeBh,
            _ => Provider::Glm,
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
// Parsed/resolved but not yet forwarded to the daemon: with the fleet client
// daemon-only (§7), per-session env/tool injection is the daemon's job. Kept so
// fleet.json `pinTools` still round-trips and can be wired into `/control/exec`.
#[allow(dead_code)]
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

    pub fn load_from(path: &Path) -> Self {
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

    /// Persist this fleet config next to the selected blackbox config.
    pub fn save(&self) -> anyhow::Result<PathBuf> {
        let path = Self::path().ok_or_else(|| anyhow::anyhow!("cannot resolve fleet.json path"))?;
        self.save_to(&path)?;
        Ok(path)
    }

    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, format!("{text}\n"))?;
        Ok(())
    }
}

impl FleetConfig {
    #[allow(dead_code)] // see DEFAULT_FLEET_PIN_TOOLS: daemon-side forwarding TODO (§7)
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

/// Opaque handle to a dispatched entrypoint agent. Wraps the live task mirror
/// (fed by the daemon status poller); the cockpit holds these and reads state
/// through [`AgentHandle::snapshot`] without touching crate-private internals.
///
/// Control flows through the daemon over HTTP (`/control/*`): `daemon` is
/// `Some` while the session is live and steerable, and `None` for a
/// reloaded/historical/failed agent that must be resumed before it can be
/// driven again. The fleet client is daemon-only — there is no in-process child
/// or local stdin pipe (harness-daemon-boundary.md §7).
#[derive(Clone)]
pub struct AgentHandle {
    task: Arc<Task>,
    daemon: Option<DaemonAgentHandle>,
}

impl AgentHandle {
    pub fn id(&self) -> String {
        self.task.id()
    }

    /// True when this agent has a live daemon session that can be steered
    /// (user-turns / interrupt over `/control/*`). False for a reloaded or
    /// terminal agent — resume it first to get a fresh live handle.
    pub fn can_steer(&self) -> bool {
        self.daemon.is_some()
    }

    /// A clone of this handle marking it non-live (a later steer resumes it
    /// rather than driving a dead session). In daemon-only mode there is no
    /// local pipe to drop, so this is a plain clone kept for cockpit API
    /// compatibility.
    pub fn without_stdin(&self) -> AgentHandle {
        self.clone()
    }

    /// Send a user-turn message (a steer / reply) into the live session (§1.1):
    /// `/control/steer`, which queues at the agent's next turn boundary if a
    /// turn is in flight.
    pub async fn send_user_turn(&self, text: &str) -> anyhow::Result<()> {
        let Some(daemon) = &self.daemon else {
            anyhow::bail!("agent has no live daemon session — resume it before steering");
        };
        daemon.steer(text).await
    }

    /// `control_request{interrupt}` — cancel the running turn (§1.1, `Esc`) via
    /// `/control/interrupt`.
    pub async fn interrupt(&self) -> anyhow::Result<()> {
        let Some(daemon) = &self.daemon else {
            anyhow::bail!("agent has no live daemon session — nothing to interrupt");
        };
        daemon.interrupt(None).await
    }

    /// `control_request{set_model}` — switch the model for subsequent turns. The
    /// daemon control plane does not yet expose live set_model (§8), so this is
    /// currently unsupported for daemon-backed fleet sessions.
    pub async fn set_model(&self, _model: &str) -> anyhow::Result<()> {
        anyhow::bail!("daemon-backed fleet sessions do not support live set_model yet")
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
            worktree_finished: stream.worktree_finished,
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
            recoverable: inner.recoverable,
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
    let json_body = content
        .find("<harness-note>")
        .map(|idx| content[..idx].trim_end())
        .unwrap_or(content);
    let value: Value = serde_json::from_str(json_body).ok()?;
    if value.get("ok").and_then(|v| v.as_bool()) == Some(false) {
        return None;
    }
    let list = value.get("list").and_then(|v| v.as_str())?;
    let items: Vec<TodoItem> = list.lines().filter_map(parse_todo_item).collect();
    if items.is_empty() {
        let total = value
            .get("total")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);
        let completed = value
            .get("completed")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(0);
        if total == 0 && completed == 0 {
            return Some(TodoState {
                total,
                completed,
                items,
            });
        }
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
    /// True once the transcript contains a successful `exit_worktree` result.
    pub worktree_finished: bool,
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
    /// Loaded/orphaned tasks are marked recoverable so the cockpit can
    /// distinguish resumable interruption from ordinary terminal state.
    pub recoverable: bool,
}

/// Harness-envelope state derived from the raw stream-json buffer.
struct StreamState {
    turn_active: bool,
    needs_input: bool,
    report_message: Option<String>,
    worktree_finished: bool,
}

/// Walk the stream-json events chronologically to recover state the daemon
/// parser doesn't track: whether a turn is in flight (toggled by `result`
/// boundaries) and the latest builtin `report` signal (§2.2). `report` lines
/// ride the harness stdout but the daemon's claude parser ignores them, so the
/// cockpit reads them here.
fn derive_stream_state(events: &[serde_json::Value]) -> StreamState {
    // A freshly dispatched agent has zero events — treat it as Active (turn in
    // flight) so it lands in the Active bucket instead of Idle until the first
    // stream event arrives. When events are non-empty the loop below overrides
    // this correctly; terminal statuses in fleet_state_from_snapshot ignore
    // turn_active entirely.
    let mut turn_active = events.is_empty();
    let mut needs_input = false;
    let mut report_message = None;
    let mut worktree_finished = false;
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for e in events {
        match e.get("type").and_then(|t| t.as_str()) {
            // A `result` closes the current turn → Idle until the next turn.
            Some("result") => turn_active = false,
            // Streaming output means a turn is running.
            Some("stream_event") => turn_active = true,
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
            Some("assistant") => {
                remember_tool_names(e, &mut tool_names);
                turn_active = true;
            }
            Some("user") => {
                if user_event_has_successful_tool_result(e, &tool_names, "exit_worktree") {
                    worktree_finished = true;
                }
                turn_active = true;
            }
            _ => {}
        }
    }
    StreamState {
        turn_active,
        needs_input,
        report_message,
        worktree_finished,
    }
}

fn remember_tool_names(e: &serde_json::Value, tool_names: &mut HashMap<String, String>) {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return;
    };
    for b in blocks {
        if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
            && let (Some(id), Some(name)) = (
                b.get("id").and_then(|id| id.as_str()),
                b.get("name").and_then(|name| name.as_str()),
            )
        {
            tool_names.insert(id.to_string(), name.to_string());
        }
    }
}

fn user_event_has_successful_tool_result(
    e: &serde_json::Value,
    tool_names: &HashMap<String, String>,
    tool_name: &str,
) -> bool {
    let Some(blocks) = e["message"]["content"].as_array() else {
        return false;
    };
    blocks.iter().any(|b| {
        if b.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            return false;
        }
        let Some(id) = b.get("tool_use_id").and_then(|id| id.as_str()) else {
            return false;
        };
        if tool_names.get(id).map(String::as_str) != Some(tool_name) {
            return false;
        }
        if b.get("is_error").and_then(|v| v.as_bool()).unwrap_or(false) {
            return false;
        }
        let content = extract_content_text(b.get("content"));
        serde_json::from_str::<Value>(&content)
            .ok()
            .and_then(|v| v.get("ok").and_then(|ok| ok.as_bool()))
            .unwrap_or(false)
    })
}

/// Best-effort model id from an `init`/assistant event in the stream-json buffer.
#[derive(Clone)]
struct DaemonFleetClient {
    base_url: Arc<str>,
    http: reqwest::Client,
}

#[derive(Clone)]
struct DaemonAgentHandle {
    client: DaemonFleetClient,
    task_id: String,
}

impl DaemonAgentHandle {
    async fn steer(&self, prompt: &str) -> anyhow::Result<()> {
        let _ = self
            .client
            .post_json(
                "/control/steer",
                json!({
                    "task_id": self.task_id,
                    "prompt": prompt,
                }),
            )
            .await?;
        Ok(())
    }

    async fn interrupt(&self, prompt: Option<&str>) -> anyhow::Result<()> {
        let mut body = json!({ "task_id": self.task_id });
        if let Some(prompt) = prompt {
            body["prompt"] = Value::String(prompt.to_string());
        }
        let _ = self.client.post_json("/control/interrupt", body).await?;
        Ok(())
    }
}

impl DaemonFleetClient {
    fn new(raw_url: impl Into<String>) -> Self {
        let mut url = raw_url.into();
        while url.ends_with('/') {
            url.pop();
        }
        if let Some(stripped) = url.strip_suffix("/mcp") {
            url = stripped.to_string();
        }
        Self {
            base_url: Arc::from(url),
            http: reqwest::Client::new(),
        }
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    async fn post_json(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let outer: Value = self
            .http
            .post(self.endpoint(path))
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        parse_tool_result_json(outer)
    }

    async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        let outer: Value = self
            .http
            .get(self.endpoint(path))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        parse_tool_result_json(outer)
    }

    fn dispatch(&self, spec: DispatchSpec, tail_tx: broadcast::Sender<TailEvent>) -> AgentHandle {
        let body = dispatch_body(&spec);
        let value = block_on_fleet_http(self.post_json("/control/exec", body))
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, tail_tx)
    }

    fn resume(&self, spec: ResumeSpec, tail_tx: broadcast::Sender<TailEvent>) -> AgentHandle {
        let body = resume_body(&spec);
        let value = block_on_fleet_http(self.post_json("/control/resume", body))
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, tail_tx)
    }

    fn handle_from_response(
        &self,
        value: Value,
        provider: Provider,
        cwd: Option<String>,
        name: Option<String>,
        tail_tx: broadcast::Sender<TailEvent>,
    ) -> AgentHandle {
        if let Some(error) = value.get("error").and_then(|v| v.as_str()) {
            return AgentHandle {
                task: daemon_task(
                    uuid::Uuid::new_v4().to_string(),
                    provider,
                    "pending".to_string(),
                    cwd,
                    name,
                    TaskStatus::Failed,
                    error.to_string(),
                ),
                daemon: None,
            };
        }
        let task_id = value
            .get("taskId")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown")
            .to_string();
        let session_id = value
            .get("sessionId")
            .and_then(|v| v.as_str())
            .unwrap_or("pending")
            .to_string();
        let task = daemon_task(
            task_id.clone(),
            provider,
            session_id,
            cwd,
            name,
            TaskStatus::Running,
            String::new(),
        );
        let daemon = DaemonAgentHandle {
            client: self.clone(),
            task_id: task_id.clone(),
        };
        spawn_daemon_status_poller(self.clone(), task.clone(), tail_tx, task_id);
        AgentHandle {
            task,
            daemon: Some(daemon),
        }
    }
}

fn block_on_fleet_http<F, T>(future: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future))
}

fn parse_tool_result_json(outer: Value) -> anyhow::Result<Value> {
    if outer.get("isError").and_then(|v| v.as_bool()) == Some(true) {
        anyhow::bail!("{}", tool_result_text(&outer));
    }
    let text = tool_result_text(&outer);
    serde_json::from_str(&text).or_else(|_| Ok(json!({ "text": text })))
}

fn tool_result_text(outer: &Value) -> String {
    outer
        .get("content")
        .and_then(|v| v.as_array())
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn dispatch_body(spec: &DispatchSpec) -> Value {
    let mut body = json!({
        "provider": spec.provider.as_str(),
        "prompt": spec.prompt,
        "allow_recursion": true,
    });
    if let Some(cwd) = &spec.cwd {
        body["project_dir"] = Value::String(cwd.clone());
    }
    if let Some(model) = &spec.model {
        body["pin_model"] = Value::String(model.clone());
    }
    if let Some(effort) = &spec.effort {
        body["pin_effort"] = Value::String(effort.clone());
    }
    body
}

fn resume_body(spec: &ResumeSpec) -> Value {
    let mut body = json!({
        "provider": spec.provider.as_str(),
        "session_id": spec.session_id,
        "prompt": spec.prompt,
        "allow_recursion": true,
    });
    if let Some(cwd) = &spec.cwd {
        body["project_dir"] = Value::String(cwd.clone());
    }
    if let Some(model) = &spec.model {
        body["pin_model"] = Value::String(model.clone());
    }
    if let Some(effort) = &spec.effort {
        body["pin_effort"] = Value::String(effort.clone());
    }
    body
}

fn daemon_task(
    id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    name: Option<String>,
    status: TaskStatus,
    stderr: String,
) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id,
            provider,
            session_id,
            events: Vec::new(),
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr,
            status,
            started_at: now_ms(),
            completed_at: status.is_terminal().then(now_ms),
            exit_code: None,
            cwd,
            bro_label: name,
            agent_label: None,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(None),
    })
}

fn spawn_daemon_status_poller(
    client: DaemonFleetClient,
    task: Arc<Task>,
    tail_tx: broadcast::Sender<TailEvent>,
    task_id: String,
) {
    tokio::spawn(async move {
        let mut last_event_count = 0usize;
        let mut terminal_sent = false;
        loop {
            let status = client
                .get_json(&format!("/control/status/{task_id}?tail=200"))
                .await;
            match status {
                Ok(value) => {
                    let terminal = update_daemon_task(&task, &value, &mut last_event_count);
                    if terminal && !terminal_sent {
                        terminal_sent = true;
                        let inner = task.inner.lock();
                        match inner.status {
                            TaskStatus::Completed => {
                                let _ = tail_tx.send(TailEvent::TaskCompleted {
                                    task_id: inner.id.clone(),
                                    elapsed: format_elapsed(inner.started_at, inner.completed_at),
                                    cost: inner.cost_usd,
                                    source_session: inner.session_id.clone(),
                                    task_kind: inner.bro_label.clone(),
                                });
                            }
                            TaskStatus::Failed => {
                                let _ = tail_tx.send(TailEvent::TaskFailed {
                                    task_id: inner.id.clone(),
                                    elapsed: format_elapsed(inner.started_at, inner.completed_at),
                                    error: inner.stderr.clone(),
                                });
                            }
                            TaskStatus::Cancelled => {
                                let _ = tail_tx.send(TailEvent::TaskCancelled {
                                    task_id: inner.id.clone(),
                                    elapsed: String::new(),
                                });
                            }
                            TaskStatus::Running => {}
                        }
                    }
                    if terminal {
                        break;
                    }
                }
                Err(err) => {
                    let mut inner = task.inner.lock();
                    inner.stderr = err.to_string();
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
    });
}

fn update_daemon_task(task: &Task, value: &Value, last_event_count: &mut usize) -> bool {
    let mut inner = task.inner.lock();
    // Prefer the typed wire snapshot (bro_protocol::TaskSnapshot) for the core
    // status plane; fall back to the legacy ad-hoc fields when an older daemon
    // omits it. This is the fleet's consumer of the contract-bottom status DTO
    // (harness-daemon-boundary.md §7).
    let typed: Option<bro_protocol::TaskSnapshot> = value
        .get("snapshot")
        .and_then(|s| serde_json::from_value(s.clone()).ok());
    if let Some(snap) = &typed {
        if let Some(sid) = &snap.session_id {
            inner.session_id = sid.as_str().to_string();
        }
        inner.status = match snap.status {
            bro_protocol::TaskStatus::Completed => TaskStatus::Completed,
            bro_protocol::TaskStatus::Failed => TaskStatus::Failed,
            bro_protocol::TaskStatus::Cancelled => TaskStatus::Cancelled,
            // Pending/Running both map to the daemon's Running.
            bro_protocol::TaskStatus::Pending | bro_protocol::TaskStatus::Running => {
                TaskStatus::Running
            }
        };
    } else {
        if let Some(session_id) = value.get("sessionId").and_then(|v| v.as_str()) {
            inner.session_id = session_id.to_string();
        }
        if let Some(status) = value.get("status").and_then(|v| v.as_str()) {
            inner.status = match status {
                "completed" => TaskStatus::Completed,
                "failed" => TaskStatus::Failed,
                "cancelled" => TaskStatus::Cancelled,
                _ => TaskStatus::Running,
            };
        }
    }
    if let Some(events) = value.get("recentEvents").and_then(|v| v.as_array()) {
        inner.events = events.clone();
    }
    let event_count = value
        .get("eventCount")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(inner.events.len());
    if event_count > *last_event_count {
        inner.supervision.last_event_at_ms = Some(now_ms());
        *last_event_count = event_count;
    }
    let mut sink = EventSink {
        last_assistant_message: None,
        usage: None,
        cost_usd: None,
        num_turns: None,
        session_id: None,
    };
    for evt in &inner.events {
        inner.provider.parse_event(evt, &mut sink);
    }
    inner.last_assistant_message = sink.last_assistant_message;
    inner.usage = sink.usage;
    inner.cost_usd = sink.cost_usd;
    inner.num_turns = sink.num_turns;
    if let Some(result) = value.get("result").and_then(|v| v.as_str())
        && !result.is_empty()
    {
        inner.last_assistant_message = Some(result.to_string());
    }
    if let Some(usage) = value.get("usage") {
        if let Some(input) = usage.get("input_tokens").and_then(|v| v.as_u64()) {
            inner
                .usage
                .get_or_insert_with(Default::default)
                .input_tokens = input;
        }
        if let Some(output) = usage.get("output_tokens").and_then(|v| v.as_u64()) {
            inner
                .usage
                .get_or_insert_with(Default::default)
                .output_tokens = output;
        }
    }
    if inner.status.is_terminal() && inner.completed_at.is_none() {
        inner.completed_at = Some(now_ms());
    }
    let terminal = inner.status.is_terminal();
    drop(inner);
    task.notify.notify_waiters();
    terminal
}

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
    /// The daemon singleton this cockpit drives. The fleet client is
    /// daemon-only — every dispatch/resume/stop is an HTTP call to `/control/*`
    /// (harness-daemon-boundary.md §7). MCP injection and per-session env are
    /// the daemon's concern, not the client's.
    daemon: DaemonFleetClient,
    /// Experimental classifier-companion config from `fleet.json`; `None` when
    /// the feature is off.
    classifier: RwLock<Option<ClassifierConfig>>,
}

impl FleetOrchestrator {
    /// Construct over an explicit `store_dir`, pointed at the default local
    /// daemon. Tests and embedders use this; the cockpit normally goes through
    /// [`FleetOrchestrator::from_config`].
    pub fn new(store_dir: PathBuf) -> Self {
        Self::with_store(store_dir, TaskStore::new(), DaemonFleetClient::new(default_daemon_url()))
    }

    fn with_store(store_dir: PathBuf, store: TaskStore, daemon: DaemonFleetClient) -> Self {
        let (tail_tx, _rx) = broadcast::channel(1024);
        Self {
            task_store: Arc::new(RwLock::new(store)),
            tail_tx,
            store_dir,
            daemon,
            classifier: RwLock::new(None),
        }
    }

    /// Build from the resolved blackbox config. The cockpit owns a **dedicated**
    /// store dir (`bro_home/fleet`), isolated from the daemon's own task store,
    /// and **loads** any prior fleet sessions from it — crashed/orphaned
    /// `Running` tasks come back as recoverable (Interrupted, §5). This is why
    /// historical sessions survive a cockpit reload.
    pub fn from_config() -> anyhow::Result<Self> {
        Self::from_config_store("fleet", None)
    }

    pub fn from_config_with_daemon_url(daemon_url: Option<String>) -> anyhow::Result<Self> {
        Self::from_config_store("fleet", daemon_url)
    }

    /// Build from the resolved blackbox config for the standalone `bro agent`
    /// shell. It uses a separate task-store subdirectory so one-off single-agent
    /// sessions do not appear in the fleet roster.
    pub fn from_agent_config() -> anyhow::Result<Self> {
        Self::from_config_store("agent", None)
    }

    fn from_config_store(store_name: &str, daemon_url: Option<String>) -> anyhow::Result<Self> {
        let cfg = crate::config::load()?;
        let store_dir = cfg.paths.bro_home.join(store_name);
        // No age-based eviction: the cockpit's model is manual cleanup (§5), so
        // historical sessions persist until explicitly deleted, not by TTL.
        let store = TaskStore::load(&store_dir, u64::MAX);
        // The fleet client is daemon-only: resolve an explicit --daemon-url,
        // then BLACKBOX_FLEET_DAEMON_URL, then the default local daemon. Dispatch
        // always rides `/control/*` against this singleton (§7).
        let url = daemon_url
            .or_else(|| std::env::var("BLACKBOX_FLEET_DAEMON_URL").ok())
            .unwrap_or_else(default_daemon_url);
        let orch = Self::with_store(store_dir, store, DaemonFleetClient::new(url));
        // The optional classifier-companion config — loaded once.
        let cfg = FleetConfig::load();
        *orch.classifier.write() = cfg.classifier.filter(ClassifierConfig::enabled_resolved);
        Ok(orch)
    }

    /// The classifier-companion config, if `fleet.json` enabled it. The cockpit
    /// reads this at dispatch time to spawn an intern session per executor.
    pub fn classifier(&self) -> Option<ClassifierConfig> {
        self.classifier.read().clone()
    }

    /// Current fleet config as the config panel should display and save it.
    pub fn fleet_config(&self) -> FleetConfig {
        let mut cfg = FleetConfig::load();
        if cfg.classifier.is_none() {
            cfg.classifier = self.classifier.read().clone().map(|mut c| {
                c.enabled = Some(true);
                c
            });
        }
        cfg
    }

    /// Update classifier config in-memory immediately and persist it to
    /// `fleet.json`. This does not restart existing executor sessions; the TUI
    /// starts/stops companions for live agents after calling this.
    pub fn set_classifier(&self, classifier: Option<ClassifierConfig>) -> anyhow::Result<PathBuf> {
        let mut cfg = FleetConfig::load();
        cfg.classifier = classifier.clone().map(|mut c| {
            c.enabled = Some(c.enabled_resolved());
            c
        });
        let path = cfg.save()?;
        *self.classifier.write() = classifier.filter(ClassifierConfig::enabled_resolved);
        Ok(path)
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
            .map(|task| AgentHandle {
                task,
                daemon: None,
            })
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
    pub fn dispatch(&self, mut spec: DispatchSpec) -> AgentHandle {
        // Fleet agents are entrypoint agents, not bros; the cockpit reuses
        // `name` as the durable roster display name (an explicit name, else the
        // prompt head) so the row survives a reload (§2.2, §5). Everything else
        // — provider args, transport env, MCP injection — is the daemon's job;
        // the client just POSTs `/control/exec` (§7).
        if spec.name.is_none() {
            spec.name = Some(prompt_head(&spec.prompt));
        }
        let handle = self.daemon.dispatch(spec, self.tail_tx.clone());
        let _ = self
            .task_store
            .write()
            .insert(handle.id(), handle.task.clone());
        handle
    }

    /// Resume a prior session (§5). Builds `--resume <id> -p <prompt>` and
    /// relaunches a persistent bidirectional session continuing the same
    /// `session_id`; the prompt is the first turn of the resumed conversation.
    /// Bidi-capable providers only.
    pub fn resume(&self, spec: ResumeSpec) -> AgentHandle {
        // Daemon-only: the daemon owns the session store and its transcript; the
        // poller repopulates the mirror task's recent events from
        // `/control/status` after the resume lands.
        let handle = self.daemon.resume(spec, self.tail_tx.clone());
        let _ = self
            .task_store
            .write()
            .insert(handle.id(), handle.task.clone());
        handle
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
        let Some(daemon) = &handle.daemon else {
            return Err("agent has no live daemon session — nothing to cancel".to_string());
        };
        let result = block_on_fleet_http(
            daemon
                .client
                .post_json("/control/cancel", json!({ "task_id": daemon.task_id })),
        )
        .map_err(|err| err.to_string());
        if result.is_ok() {
            let mut inner = handle.task.inner.lock();
            inner.status = TaskStatus::Cancelled;
            inner.completed_at = Some(now_ms());
            handle.task.notify.notify_waiters();
        }
        result.map(|_| ())
    }

}

/// First line of the prompt, capped — the durable display name for a session.
fn prompt_head(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("").trim();
    line.chars().take(60).collect()
}

/// The default daemon base URL the cockpit drives when no `--daemon-url` /
/// `BLACKBOX_FLEET_DAEMON_URL` is given: the local daemon on `BBOX_PORT`
/// (default 7264). The fleet client is daemon-only, so `bro fleet` with no flags
/// targets the local singleton (harness-daemon-boundary.md §7).
fn default_daemon_url() -> String {
    let port = std::env::var("BBOX_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
        .unwrap_or(7264);
    format!("http://127.0.0.1:{port}")
}

/// Providers that speak the persistent bidirectional stream-json control
/// protocol: the bro-harness providers GLM / DeepSeek / Brodex / VibeBh (§2).
/// Others are one-shot only. Public so the
/// cockpit can tell whether a non-live agent is resumable.
pub fn provider_supports_bidi(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Glm | Provider::Deepseek | Provider::Brodex | Provider::VibeBh
    )
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
    fn fleet_config_parses_project_alias_map() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{
                "projects": {
                    "blackbox": "/Users/me/repos/transcript-search",
                    "tools": "/Users/me/repos/transcript-search/crates/bro-tools"
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            cfg.projects.get("blackbox").map(String::as_str),
            Some("/Users/me/repos/transcript-search")
        );
        assert_eq!(
            cfg.projects.get("tools").map(String::as_str),
            Some("/Users/me/repos/transcript-search/crates/bro-tools")
        );
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
    fn bidi_capability_gate() {
        for p in [
            Provider::Glm,
            Provider::Deepseek,
            Provider::Brodex,
            Provider::VibeBh,
        ] {
            assert!(provider_supports_bidi(p), "{p} should be bidi-capable");
        }
        assert!(!provider_supports_bidi(Provider::Workflow));
    }

    #[test]
    fn stream_state_empty_events_means_active() {
        // A freshly dispatched agent has zero events — it should appear Active
        // (turn in flight) so the roster shows it in the Active bucket rather
        // than Idle until the first stream event arrives.
        let s = derive_stream_state(&[]);
        assert!(s.turn_active, "empty events should mean turn_active=true");
        assert!(!s.needs_input);
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
    fn transcript_parses_todo_write_result_with_harness_note_as_todo_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":2,\"completed\":2,\"list\":\"[x] Done\\n[x] Also done\"}\n\n<harness-note>consider clearing exhaust</harness-note>","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 2, completed: 2, items })
                if items.len() == 2 && items.iter().all(|i| i.status == TodoItemStatus::Completed)
        ));
    }

    #[test]
    fn transcript_parses_empty_todo_write_as_clear_state() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"todo1","name":"todo_write","input":{"items":[]}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"todo1","content":"{\"ok\":true,\"total\":0,\"completed\":0,\"list\":\"(todo list is empty)\"}","is_error":false}
            ]}}"#),
        ];
        let items = parse_transcript(&events);
        assert!(matches!(
            &items[1],
            TranscriptItem::TodoState(TodoState { total: 0, completed: 0, items })
                if items.is_empty()
        ));
    }

    #[test]
    fn stream_state_detects_successful_exit_worktree() {
        let ev = |s: &str| -> Value { serde_json::from_str(s).unwrap() };
        let events = vec![
            ev(r#"{"type":"assistant","message":{"content":[
                {"type":"tool_use","id":"exit1","name":"exit_worktree","input":{"disposition":"publish"}}
            ]}}"#),
            ev(r#"{"type":"user","message":{"role":"user","content":[
                {"type":"tool_result","tool_use_id":"exit1","content":"{\"ok\":true,\"disposition\":\"publish\"}","is_error":false}
            ]}}"#),
            ev(r#"{"type":"result","subtype":"success"}"#),
        ];
        let stream = derive_stream_state(&events);
        assert!(stream.worktree_finished);
        assert!(!stream.turn_active);
    }

    #[test]
    fn dispatch_spec_builder_defaults() {
        let spec = DispatchSpec::new(Provider::Glm, "hello");
        assert_eq!(spec.prompt, "hello");
        assert!(spec.cwd.is_none());
        assert!(spec.model.is_none());
    }

    #[test]
    fn fleet_config_parses_classifier() {
        let cfg: FleetConfig = serde_json::from_str(
            r#"{ "classifier": { "enabled": true, "provider": "glm", "cadence_secs": 8, "auto_send": false } }"#,
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
    fn fleet_config_classifier_presence_alone_is_not_enabled() {
        let cfg: FleetConfig =
            serde_json::from_str(r#"{ "classifier": { "provider": "glm" } }"#).unwrap();
        let c = cfg.classifier.expect("classifier config stays available");
        assert!(!c.enabled_resolved());
    }

    #[test]
    fn fleet_config_persists_explicit_classifier_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().canonicalize().unwrap().join("fleet.json");
        let cfg = FleetConfig {
            classifier: Some(ClassifierConfig {
                enabled: Some(false),
                provider: Some("glm".into()),
                ..ClassifierConfig::default()
            }),
            ..FleetConfig::default()
        };

        cfg.save_to(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains(r#""enabled": false"#), "{text}");

        let loaded = FleetConfig::load_from(&path);
        let c = loaded.classifier.expect("classifier object stays present");
        assert!(!c.enabled_resolved());
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
    fn classifier_provider_defaults_to_glm_and_stays_bidi() {
        // Unknown / non-bidi names — and `claude`, which is no longer a fleet
        // participant — collapse to GLM. The classifier must stay steerable, so
        // it can never resolve to a one-shot (or non-fleet) provider.
        for name in [
            None,
            Some("claude"),
            Some("codex"),
            Some("gemini"),
            Some("nonsense"),
        ] {
            let c = ClassifierConfig {
                enabled: None,
                provider: name.map(str::to_string),
                model: None,
                effort: None,
                prompt: None,
                cadence_secs: None,
                auto_send: None,
                min_activity: None,
            };
            assert_eq!(c.provider_resolved(), Provider::Glm);
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

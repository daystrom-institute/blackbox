//! `FleetOrchestrator` — the daemon-driving façade the `bro fleet` cockpit uses.
//!
//! The fleet client is **daemon-only** (harness-daemon-boundary.md §7): every
//! dispatch/resume/stop is an HTTP call to the daemon singleton's `/control/*`
//! routes. This crate links only the contract bottom (`bro-protocol` +
//! `bro-core`) plus its own transport/runtime deps — never the `blackbox`
//! daemon crate. View-local mirror state (the `Task` store the cockpit reloads,
//! the live transcript parse) lives here; system state lives in the daemon.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{Notify, broadcast};

use crate::config;
use crate::events::{EventSink, parse_claude_event};
use crate::mcp::McpServerConfig;
use crate::tail::TailEvent;
use crate::task::{Task, TaskInner, TaskStore, format_elapsed, now_ms};

// The contract-bottom view/wire DTOs (harness-daemon-boundary.md §2/§7). Status
// is the wire `bro_protocol::TaskStatus` directly — there is no daemon-internal
// enum on the client side.
pub use bro_core::Provider;
pub use bro_protocol::{CloseoutOutcome, CloseoutRequest, DispatchSpec, ResumeSpec, TaskStatus, TodoItem, TodoItemStatus, TodoState, TranscriptItem};

/// TUI-local fleet config — `fleet.json` beside the selected blackbox
/// `config.toml` but read entirely daemon-free. Deliberately
/// **not** the bbox project registry or the daemon's `mcp.json`: those drag in
/// the daemon plus per-project indexing, inappropriate for the cockpit.
///
/// `mcpServers` / `pinTools` are parsed and round-tripped but not interpreted by
/// the client: since 1b the daemon owns MCP/tool injection, so these are kept so
/// a user's `fleet.json` survives a config-panel save and can later be forwarded
/// to `/control/exec`.
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
    /// agent. Round-tripped but not interpreted by the client (daemon owns tool
    /// injection since 1b).
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

    /// Default code-mode for new roster dispatches (`off`/`optional`/`only`),
    /// toggled via `/config`. Forwarded on `DispatchSpec.code_mode` for fresh
    /// sessions only; resume relies on the session's persisted value. Absent →
    /// harness default (`optional`). Round-tripped but not interpreted by the
    /// client (the daemon/harness own code-mode semantics).
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_mode: Option<String>,
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
            Some("minimax") | Some("mmx") => Provider::Minimax,
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
        config::selected_config_path().and_then(|p| p.parent().map(|d| d.join("fleet.json")))
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

/// Opaque handle to a dispatched entrypoint agent. Wraps the live task mirror
/// (fed by the daemon status poller); the cockpit holds these and reads state
/// through [`AgentHandle::snapshot`].
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
    ///
    /// A daemon handle alone is not enough: it is set at dispatch and never
    /// cleared on completion, so `daemon.is_some()` stays true for a finished
    /// agent. The daemon's `/control/steer` only accepts a `Running` task
    /// (`bro_steer`: "task … is {Status}, not running"), so we mirror that
    /// exact condition. Otherwise the cockpit would steer a `Completed`
    /// in-process agent, the daemon would reject it, and — because that
    /// rejection is not a broken pipe — `steer_selected`'s resume fallback
    /// would never fire (the §7 cut made fleet agents one-shot-per-dispatch;
    /// multi-turn rides resume, not a persistent stdin session).
    pub fn can_steer(&self) -> bool {
        self.daemon.is_some() && matches!(self.task.inner.lock().status, TaskStatus::Running)
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
        self.interrupt_redirect(None).await
    }

    /// Interrupt the running turn and, when `redirect` is `Some`, deliver it as
    /// the immediate next turn (halt-and-redirect): the harness cancels the
    /// model call, then `pending.push_front`s the redirect so it runs right away
    /// — distinct from [`send_user_turn`], which interleaves at the next natural
    /// boundary without cancelling.
    pub async fn interrupt_redirect(&self, redirect: Option<&str>) -> anyhow::Result<()> {
        let Some(daemon) = &self.daemon else {
            anyhow::bail!("agent has no live daemon session — nothing to interrupt");
        };
        daemon.interrupt(redirect).await
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
            // The harness `report` line. (The daemon BroReport fallback that
            // existed in-process never populated for fleet agents.)
            report_message: stream.report_message,
            needs_input: stream.needs_input,
            turn_active: stream.turn_active,
            worktree_finished: stream.worktree_finished,
            cost_usd: inner.cost_usd,
            num_turns: inner.num_turns,
            started_at: inner.started_at,
            // Wall-clock of the last observed stream event — "last interaction",
            // a roster timing column + sort axis. Stamped by the poller on every
            // event-count increase.
            last_event_at_ms: inner.last_event_at_ms,
            cwd: inner.cwd.clone(),
            stderr: inner.stderr.clone(),
            // Prefer the model persisted at dispatch time (survives reload for
            // providers whose stream events lack a top-level `model` field).
            model: inner.model.clone().or_else(|| model_from_events(&inner.events)),
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

/// Opening marker of the window-0 diagnostics rider the harness appends to a
/// tool-result body. WIRE CONTRACT: must match `RIDER_MARKER` in
/// `crates/bro-harness/src/diagnostics/render.rs` (the string is duplicated
/// rather than shared).
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
/// into a task without naming the mirror `Task`/`TaskInner`.
#[derive(Debug, Clone)]
pub struct TaskSnapshot {
    pub status: TaskStatus,
    pub provider: Provider,
    pub session_id: String,
    pub last_assistant_message: Option<String>,
    /// Latest status line from the harness `report` tool.
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
            // Bound every request so a saturated daemon can never hang a caller
            // indefinitely. The status poller (`spawn_daemon_status_poller`) is
            // the cockpit's liveness path: without a timeout a single stalled
            // `/control/status` poll froze the whole roster at its last state
            // (observed 28min stale during heavy in-process agent work). The
            // global 180s backstop is generous enough for the longest control
            // op (a `/control/closeout` rebase+push); the status poll adds a far
            // shorter per-request timeout below so it recovers fast.
            http: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(10))
                .timeout(std::time::Duration::from_secs(180))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
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

    /// POST raw JSON to a `/control/*` endpoint that returns the wire DTO
    /// directly (no tool-result envelope). The Phase 3a `/control/closeout`
    /// handler returns the structured `CloseoutOutcome` as `axum::Json` —
    /// distinct from `/control/exec` and friends, which return
    /// `CallToolResult` envelopes that `post_json` unwraps. Guard/validation
    /// failures still come back as non-2xx with a plain `{"error": "..."}`
    /// body (the handler uses `axum::response::IntoResponse`); those are
    /// surfaced as `Err` from `error_for_status`, so the caller can match
    /// the daemon's structured outcome on `Ok` and treat HTTP failures
    /// uniformly.
    async fn post_json_value(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        let resp = self
            .http
            .post(self.endpoint(path))
            .json(&body)
            .send()
            .await?
            .error_for_status()?;
        Ok(resp.json().await?)
    }

    async fn get_json(&self, path: &str) -> anyhow::Result<Value> {
        // A short per-request timeout (tighter than the client-wide backstop) so
        // the status poller recovers fast: GETs here are cheap reads, so if one
        // stalls on a half-open keepalive connection while the daemon is briefly
        // CPU-starved by an in-process agent's build, it errors in ~15s and the
        // poll loop retries on a fresh connection instead of freezing the roster.
        let outer: Value = self
            .http
            .get(self.endpoint(path))
            .timeout(std::time::Duration::from_secs(15))
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
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model, tail_tx)
    }

    fn resume(&self, spec: ResumeSpec, tail_tx: broadcast::Sender<TailEvent>) -> AgentHandle {
        let body = resume_body(&spec);
        let value = block_on_fleet_http(self.post_json("/control/resume", body))
            .unwrap_or_else(|err| json!({ "error": err.to_string() }));
        self.handle_from_response(value, spec.provider, spec.cwd, spec.name, spec.model, tail_tx)
    }

    /// Drive `/control/closeout` for a focused fleet agent. The endpoint
    /// returns the structured `CloseoutOutcome` directly (no transcript
    /// parsing — design/fleet-tui/closeout-command.md §4.3). Used by the
    /// cockpit `/closeout <disposition> [--dry-run]` command (Phase 3b).
    fn closeout(&self, req: &CloseoutRequest) -> anyhow::Result<CloseoutOutcome> {
        let value: Value = block_on_fleet_http(self.post_json_value(
            "/control/closeout",
            serde_json::to_value(req)?,
        ))?;
        Ok(serde_json::from_value(value)?)
    }

    fn handle_from_response(
        &self,
        value: Value,
        provider: Provider,
        cwd: Option<String>,
        name: Option<String>,
        model: Option<String>,
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
                    model,
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
            model,
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
    if let Some(code_mode) = &spec.code_mode {
        body["code_mode"] = Value::String(code_mode.clone());
    }
    if let Some(service_tier) = &spec.service_tier {
        body["service_tier"] = Value::String(service_tier.clone());
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

#[allow(clippy::too_many_arguments)]
fn daemon_task(
    id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    name: Option<String>,
    model: Option<String>,
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
            cost_usd: None,
            num_turns: None,
            stderr,
            status,
            started_at: now_ms(),
            completed_at: status.is_terminal().then(now_ms),
            cwd,
            bro_label: name,
            recoverable: false,
            last_event_at_ms: None,
            model,
        }),
        notify: Arc::new(Notify::new()),
        last_poll_ms: AtomicU64::new(now_ms()),
    })
}

/// How many recent events the status poll requests. Kept well below the count
/// at which a verbose agent's `/control/status` response (the per-event payloads
/// can be large) exceeds the daemon's 80KB response cap (`server/response.rs`),
/// which BYTE-truncates the body and corrupts the JSON — the cockpit then can't
/// parse the status and the row silently freezes (the root cause of the observed
/// "poller stall"). 80 events keeps the response valid for the verbose agents
/// seen in practice; the daemon-side fix is to bound per-event content so the
/// status JSON is always valid regardless of tail. Empirically a very verbose
/// agent (deepseek-v4, ~1.2KB/event) truncates at tail=80 (88KB) but is clean at
/// tail=40 (47KB), so 40 is the conservative client cap until the daemon fix
/// lands. (An agent verbose enough to blow 80KB at tail=40 would still break —
/// only the daemon-side per-event content bound fully closes this.)
const POLL_TAIL: usize = 40;

fn spawn_daemon_status_poller(
    client: DaemonFleetClient,
    task: Arc<Task>,
    tail_tx: broadcast::Sender<TailEvent>,
    task_id: String,
) {
    tokio::spawn(async move {
        let mut last_event_count = 0usize;
        let mut terminal_sent = false;
        // Consecutive failed polls (transport error or status-parse panic) since
        // the last good one. Bounds a zombie poller: if the daemon has genuinely
        // lost the task (it restarted, or the task was pruned) every poll fails
        // forever, so after a sustained window we stop and mark the mirror
        // recoverable — a fresh launch's `reconcile_reloaded` pass is the
        // recovery path. ~3 min at 750ms: long enough to ride out a daemon
        // briefly CPU-starved by an in-process build, short enough not to 404 in
        // a tight loop indefinitely.
        let mut consecutive_failures: u32 = 0;
        // Polls that returned HTTP-OK but carried NO parseable status — the
        // signature of an 80KB-cap-truncated (corrupt-JSON) response for a very
        // verbose agent. The status can't be updated from such a response; make
        // it LOUD (log + the agent's own stderr) instead of silently freezing.
        let mut unparseable_polls: u32 = 0;
        const MAX_CONSECUTIVE_FAILURES: u32 = 240;
        tracing::debug!(task_id = %task_id, "fleet status poller started");
        loop {
            let status = client
                .get_json(&format!("/control/status/{task_id}?tail={POLL_TAIL}"))
                .await;
            // Liveness heartbeat: this poll cycle completed (ok or error), so the
            // poller task is demonstrably alive. The supervisor reads this to
            // distinguish a live-but-quiet poller from a silently-wedged one.
            task.mark_polled();
            match status {
                Ok(value) => {
                    consecutive_failures = 0;
                    // An HTTP-OK response that carries no parseable status is the
                    // 80KB-truncation signature — surface it loudly rather than
                    // letting update_daemon_task silently no-op the status.
                    if parse_daemon_status(&value).is_none() {
                        unparseable_polls += 1;
                        if unparseable_polls == 1 || unparseable_polls.is_multiple_of(40) {
                            tracing::warn!(
                                task_id = %task_id,
                                count = unparseable_polls,
                                "fleet status poller: /control/status returned no parseable \
                                 status (likely an 80KB-cap-truncated response for a verbose \
                                 agent); status not updated this poll"
                            );
                            let mut inner = task.inner.lock();
                            inner.stderr = format!(
                                "[poller] daemon status response unparseable/truncated ×{unparseable_polls}; \
                                 displayed status may be stale"
                            );
                        }
                    } else {
                        unparseable_polls = 0;
                    }
                    // The parse walks untrusted daemon JSON (arbitrary event
                    // shapes from any provider). A panic here used to kill the
                    // poller task silently and freeze THIS agent's roster row
                    // forever while the rest of the cockpit stayed live — the
                    // "poller death" defect. Isolate it: a bad event costs one
                    // poll, not the whole liveness loop. parking_lot does not
                    // poison on unwind, so the next poll re-locks cleanly. (The
                    // workspace pins `panic = "unwind"`, so catch_unwind works.)
                    let parsed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        update_daemon_task(&task, &value, &mut last_event_count)
                    }));
                    let terminal = match parsed {
                        Ok(terminal) => terminal,
                        Err(_) => {
                            tracing::error!(
                                task_id = %task_id,
                                "fleet status poller panicked parsing daemon status; \
                                 skipped this poll, poller stays alive"
                            );
                            // Loud but TUI-safe: surface into the agent's own
                            // stderr (rendered in the detail view).
                            let mut inner = task.inner.lock();
                            inner.stderr = "[poller] recovered from a panic parsing daemon \
                                            status (this poll skipped; live tracking continues)"
                                .to_string();
                            false
                        }
                    };
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
                            TaskStatus::Running | TaskStatus::Pending => {}
                        }
                    }
                    if terminal {
                        break;
                    }
                }
                Err(err) => {
                    consecutive_failures += 1;
                    // Loud on the first failure, then sparse — both to the log
                    // and into the agent's own stderr (detail view).
                    if consecutive_failures == 1 || consecutive_failures.is_multiple_of(40) {
                        tracing::warn!(
                            task_id = %task_id,
                            failures = consecutive_failures,
                            "fleet status poller: /control/status failed: {err:#}"
                        );
                        let mut inner = task.inner.lock();
                        inner.stderr =
                            format!("[poller] /control/status failing ({consecutive_failures}×): {err}");
                    }
                    if consecutive_failures >= MAX_CONSECUTIVE_FAILURES {
                        tracing::error!(
                            task_id = %task_id,
                            failures = consecutive_failures,
                            "fleet status poller giving up; daemon lost the task or is \
                             unreachable. Marking recoverable."
                        );
                        let mut inner = task.inner.lock();
                        inner.status = TaskStatus::Failed;
                        inner.recoverable = true;
                        inner.completed_at = Some(now_ms());
                        inner.stderr = format!(
                            "[poller] gave up after {consecutive_failures} consecutive \
                             /control/status failures — daemon lost the task or is \
                             unreachable. Resume to re-attach a live session."
                        );
                        drop(inner);
                        task.notify.notify_waiters();
                        break;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(750)).await;
        }
        tracing::debug!(task_id = %task_id, "fleet status poller exited");
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
        inner.status = snap.status;
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
        inner.last_event_at_ms = Some(now_ms());
        *last_event_count = event_count;
    }
    let mut sink = EventSink::default();
    for evt in &inner.events {
        parse_claude_event(evt, &mut sink);
    }
    inner.last_assistant_message = sink.last_assistant_message;
    inner.cost_usd = sink.cost_usd;
    inner.num_turns = sink.num_turns;
    if let Some(result) = value.get("result").and_then(|v| v.as_str())
        && !result.is_empty()
    {
        inner.last_assistant_message = Some(result.to_string());
    }
    if inner.status.is_terminal() && inner.completed_at.is_none() {
        inner.completed_at = Some(now_ms());
    }
    // Backfill model from events for tasks that predate the persisted-model
    // field (or were created without a dispatch-time model pin).
    if inner.model.is_none() {
        inner.model = model_from_events(&inner.events);
    }
    let terminal = inner.status.is_terminal();
    drop(inner);
    task.notify.notify_waiters();
    terminal
}

/// Extract the daemon's authoritative task status from a `/control/status`
/// response, mirroring [`update_daemon_task`]'s precedence: the typed wire
/// snapshot (`bro_protocol::TaskSnapshot`) first, then the legacy top-level
/// `status` string. Returns `None` when the response carried NO parseable
/// status — e.g. a truncated/unwrapped envelope a saturated daemon returned, or
/// the `{"text":...}` fallback from `parse_tool_result_json`. Callers MUST treat
/// `None` as inconclusive (attach a poller), never as terminal.
fn parse_daemon_status(value: &Value) -> Option<TaskStatus> {
    if let Some(snap) = value
        .get("snapshot")
        .and_then(|s| serde_json::from_value::<bro_protocol::TaskSnapshot>(s.clone()).ok())
    {
        return Some(snap.status);
    }
    match value.get("status").and_then(|v| v.as_str()) {
        Some("completed") => Some(TaskStatus::Completed),
        Some("failed") => Some(TaskStatus::Failed),
        Some("cancelled") => Some(TaskStatus::Cancelled),
        Some("running") => Some(TaskStatus::Running),
        Some("pending") => Some(TaskStatus::Pending),
        _ => None,
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

/// Daemon-driving orchestration core for the fleet cockpit. Holds the
/// `TaskStore` mirror, the tail broadcast channel, and the on-disk `store_dir`
/// the cockpit reloads from.
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
        Self::with_store(
            store_dir,
            TaskStore::new(),
            DaemonFleetClient::new(default_daemon_url()),
        )
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
        let store_dir = config::bro_home().join(store_name);
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

    /// Persist the fleet-wide default code-mode (`off`/`optional`/`only`, or
    /// `None` to clear → harness default) to `fleet.json`. Mirrors
    /// [`set_classifier`](Self::set_classifier): load → modify → save, so the
    /// other fleet config fields are preserved. New roster dispatches read this
    /// onto `DispatchSpec.code_mode`.
    pub fn set_code_mode(&self, code_mode: Option<String>) -> anyhow::Result<PathBuf> {
        let mut cfg = FleetConfig::load();
        cfg.code_mode = code_mode;
        cfg.save()
    }

    /// Flush the current task store to disk so a later cockpit launch can
    /// reload these sessions. The cockpit calls this after each dispatch and on
    /// quit.
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
            .map(|task| AgentHandle { task, daemon: None })
            .collect()
    }

    /// Spawn the poller-supervisor: ONE long-lived watchdog task that respawns
    /// any per-task status poller that has silently wedged.
    ///
    /// The per-task pollers can stop making progress under load WITHOUT
    /// panicking or erroring — observed live: pollers frozen ~10min while the
    /// daemon stayed healthy (15ms `/control/status`) and the render thread
    /// stayed alive, so the cockpit showed a finished agent as still "Active"
    /// and a running agent as frozen. A panic-isolated, logging poller (the A2
    /// hardening) does nothing when the task simply stops being driven; this
    /// liveness backstop is what actually keeps the roster honest. (The deeper
    /// fix — a single daemon-authoritative reconciling pull / SSE instead of N
    /// fragile client pollers — is the eventual architecture; this watchdog is
    /// the robust, low-risk first cut.) Call once, after the initial reconcile.
    pub fn spawn_poller_supervisor(&self) {
        let task_store = self.task_store.clone();
        let daemon = self.daemon.clone();
        let tail_tx = self.tail_tx.clone();
        tokio::spawn(async move {
            // Heartbeat staleness past which a poller is presumed wedged. The
            // poller beats every ~750ms (up to ~15s on a timed-out poll), so 30s
            // is clear of a healthy slow poll yet catches a true stall fast.
            const STALL_MS: u64 = 30_000;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                // Snapshot the stalled non-terminal tasks under a brief read lock.
                let stalled: Vec<Arc<Task>> = {
                    let store = task_store.read();
                    store
                        .all_tasks()
                        .into_iter()
                        .filter(|t| !t.inner.lock().status.is_terminal())
                        .filter(|t| t.since_last_poll_ms() > STALL_MS)
                        .collect()
                };
                for task in stalled {
                    let id = task.id();
                    tracing::warn!(
                        task_id = %id,
                        stalled_ms = task.since_last_poll_ms(),
                        "poller-supervisor: status poller heartbeat stale; respawning poller"
                    );
                    // Reset the heartbeat so we don't respawn again on the next
                    // tick before the fresh poller has had a chance to beat.
                    task.mark_polled();
                    spawn_daemon_status_poller(daemon.clone(), task.clone(), tail_tx.clone(), id);
                }
            }
        });
    }

    pub fn store_dir(&self) -> &std::path::Path {
        &self.store_dir
    }

    /// Reconcile reloaded sessions against the daemon once at cockpit startup —
    /// the live-tracking-aware replacement for a bare [`tasks`](Self::tasks)
    /// enumeration on launch.
    ///
    /// [`TaskStore::load`] eagerly flips every persisted `Running` task to
    /// `Failed`+recoverable ("Interrupted"). Under the in-process model that is
    /// often wrong: the daemon may STILL be running the agent and only the
    /// cockpit restarted. Without reconciliation those agents show frozen at
    /// their last-persisted state forever and have no poller — the reload half
    /// of the "poller death" defect. This pass asks the daemon for the truth of
    /// each recoverable task:
    ///
    /// * daemon `Running`/`Pending` → restore the mirror to live (status back to
    ///   running, `recoverable` cleared, `completed_at` cleared), attach a fresh
    ///   status poller, and hand back a **steerable** handle (`daemon: Some`).
    ///   This is what restores live tracking across a cockpit reload.
    /// * daemon terminal → adopt that final status (it finished while the cockpit
    ///   was down); not recoverable, not steerable.
    /// * daemon unknown / unreachable → leave it Interrupted (genuinely orphaned
    ///   — the daemon also restarted, or the task was pruned); resume to recover.
    ///
    /// Tasks already terminal at load are returned as-is with no round-trip.
    pub async fn reconcile_reloaded(&self) -> Vec<AgentHandle> {
        let all = self.task_store.read().all_tasks();
        let mut out: Vec<AgentHandle> = Vec::with_capacity(all.len());
        let mut probes = tokio::task::JoinSet::new();
        for task in all {
            // Only tasks the loader marked recoverable (i.e. were Running at
            // persist) need a daemon round-trip; everything else is already a
            // settled terminal/historical row.
            if task.inner.lock().recoverable {
                let client = self.daemon.clone();
                probes.spawn(async move {
                    let id = task.id();
                    let status = client
                        .get_json(&format!("/control/status/{id}?tail={POLL_TAIL}"))
                        .await;
                    (task, status)
                });
            } else {
                out.push(AgentHandle { task, daemon: None });
            }
        }

        while let Some(joined) = probes.join_next().await {
            let Ok((task, status)) = joined else {
                // A probe task itself panicked/cancelled — treat as unreachable.
                continue;
            };
            let id = task.id();
            match status {
                Ok(value) => {
                    // Extract the daemon's AUTHORITATIVE status explicitly,
                    // BEFORE the side-effecting parse. Only an explicit terminal
                    // verdict may retire the task; an explicit running verdict
                    // restores it; and a MISSING/unparseable status is
                    // INCONCLUSIVE — never terminal. Under load the daemon's
                    // `/control/status` can time out or return a truncated
                    // envelope, in which case `parse_tool_result_json` falls back
                    // to `{"text":...}` with no status field. Treating that as
                    // terminal once stranded a live brodex bro as Interrupted
                    // forever: it set recoverable=false, which made every later
                    // reconcile skip the task. Inconclusive ⇒ optimistic poller,
                    // exactly like a failed probe.
                    let daemon_status = parse_daemon_status(&value);
                    // Still run the side-effecting parse to populate events /
                    // model / message (it leaves status untouched when absent).
                    let mut dummy = 0usize;
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        update_daemon_task(&task, &value, &mut dummy)
                    }));
                    let live = !matches!(daemon_status, Some(s) if s.is_terminal());
                    {
                        let mut inner = task.inner.lock();
                        inner.recoverable = false;
                        // Clear the bogus reconcile-time `last_event_at_ms` stamp
                        // (update_daemon_task saw the whole persisted backlog as
                        // "new"); the re-attached poller re-stamps it on the next
                        // genuine event, and a terminal row falls back to the real
                        // start age in the "last" column.
                        inner.last_event_at_ms = None;
                        if live {
                            inner.completed_at = None;
                        }
                    }
                    if live {
                        tracing::info!(
                            task_id = %id,
                            terminal = false,
                            "reconcile: reloaded task is live or inconclusive on the \
                             daemon; attaching poller to settle true state"
                        );
                        spawn_daemon_status_poller(
                            self.daemon.clone(),
                            task.clone(),
                            self.tail_tx.clone(),
                            id.clone(),
                        );
                        out.push(AgentHandle {
                            task,
                            daemon: Some(DaemonAgentHandle {
                                client: self.daemon.clone(),
                                task_id: id,
                            }),
                        });
                    } else {
                        tracing::info!(
                            task_id = %id,
                            "reconcile: reloaded task is terminal on the daemon; \
                             adopted final status"
                        );
                        out.push(AgentHandle { task, daemon: None });
                    }
                }
                Err(err) => {
                    // The probe FAILED — but that does NOT mean the task is dead.
                    // At cockpit relaunch the daemon is often momentarily slow
                    // (many recoverable tasks probed at once + in-process build
                    // load), so a single timed-out `/control/status` would
                    // otherwise STRAND a still-running agent as Interrupted
                    // forever (one-shot reconcile, no poller, no retry — observed:
                    // a live brodex bro shown Interrupted while the daemon had it
                    // running). Optimistically re-attach a poller: if the task is
                    // alive its next successful poll restores Running + live
                    // tracking; if it is genuinely gone the poller's bounded
                    // give-up (consecutive-failure cap) retires it cleanly. Hand
                    // back a steerable handle so it can be driven once restored.
                    tracing::info!(
                        task_id = %id,
                        "reconcile: status probe failed ({err:#}); attaching a poller \
                         optimistically (the task may still be live)"
                    );
                    spawn_daemon_status_poller(
                        self.daemon.clone(),
                        task.clone(),
                        self.tail_tx.clone(),
                        id.clone(),
                    );
                    out.push(AgentHandle {
                        task,
                        daemon: Some(DaemonAgentHandle {
                            client: self.daemon.clone(),
                            task_id: id,
                        }),
                    });
                }
            }
        }
        out
    }

    /// Spawn a new top-level entrypoint agent over the daemon control plane.
    /// Returns an [`AgentHandle`] — the cockpit holds it to read state and drive
    /// the session.
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
        self.task_store
            .write()
            .insert(handle.id(), handle.task.clone());
        handle
    }

    /// Resume a prior session (§5) over the daemon control plane: the daemon
    /// owns the session store and its transcript; the poller repopulates the
    /// mirror task's recent events from `/control/status` after the resume lands.
    pub fn resume(&self, spec: ResumeSpec) -> AgentHandle {
        let handle = self.daemon.resume(spec, self.tail_tx.clone());
        self.task_store
            .write()
            .insert(handle.id(), handle.task.clone());
        handle
    }

    /// Drive `/control/closeout` for a focused fleet agent. The daemon returns
    /// the structured `CloseoutOutcome` directly (no transcript parsing —
    /// design/fleet-tui/closeout-command.md §4.3). Used by the cockpit
    /// `/closeout <disposition> [--dry-run]` command (Phase 3b). The
    /// orchestrator doesn't manage any new task state here: closeout is a
    /// single-fire HTTP call, not a registered agent.
    pub fn closeout(&self, req: &CloseoutRequest) -> anyhow::Result<CloseoutOutcome> {
        self.daemon.closeout(req)
    }

    /// Drop a task from the store (used after a resume supersedes the old
    /// Interrupted task, or on Ctrl+X delete, so a reload doesn't show it). The
    /// underlying provider session jsonl survives on disk regardless (§5).
    pub fn forget(&self, task_id: &str) {
        self.task_store.write().retain_drop(|t| t.id() != task_id);
        self.persist();
    }

    /// Stop a running session (Ctrl+X): the daemon SIGTERMs the child and marks
    /// it Cancelled (→ Interrupted). The provider session survives on disk for
    /// resume.
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
/// Others are one-shot only. Public so the cockpit can tell whether a non-live
/// agent is resumable.
pub fn provider_supports_bidi(provider: Provider) -> bool {
    matches!(
        provider,
        Provider::Glm | Provider::Deepseek | Provider::Minimax | Provider::Brodex | Provider::VibeBh
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
    fn dispatch_body_carries_code_mode_only_when_set() {
        let mut spec = DispatchSpec::new(Provider::Glm, "hi");
        // Absent ⇒ no code_mode/service_tier keys (harness applies defaults).
        assert!(dispatch_body(&spec).get("code_mode").is_none());
        assert!(dispatch_body(&spec).get("service_tier").is_none());
        // Set ⇒ forwarded as ExecParams.code_mode.
        spec.code_mode = Some("only".to_string());
        spec.service_tier = Some("priority".to_string());
        assert_eq!(
            dispatch_body(&spec).get("code_mode").and_then(|v| v.as_str()),
            Some("only")
        );
        assert_eq!(
            dispatch_body(&spec)
                .get("service_tier")
                .and_then(|v| v.as_str()),
            Some("priority")
        );
    }

    #[test]
    fn fleet_config_code_mode_round_trips() {
        let cfg: FleetConfig = serde_json::from_str(r#"{"code_mode":"only"}"#).unwrap();
        assert_eq!(cfg.code_mode.as_deref(), Some("only"));
        // Absent ⇒ None (harness default), and it's skipped when serializing.
        let bare: FleetConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(bare.code_mode, None);
        assert!(!serde_json::to_string(&bare).unwrap().contains("code_mode"));
    }

    #[test]
    fn fleet_config_round_trips_mcp_servers() {
        // The client doesn't interpret mcpServers but MUST preserve them across a
        // save — model the secret/header values as opaque JSON so a config-panel
        // write doesn't drop a user's fleet.json mcpServers.
        let src = r#"{
            "mcpServers": {
                "ctx": { "type": "http", "url": "https://x/mcp",
                         "headers": { "Authorization": { "$secret": "TOKEN" } } }
            }
        }"#;
        let cfg: FleetConfig = serde_json::from_str(src).unwrap();
        let out = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            out["mcpServers"]["ctx"]["headers"]["Authorization"]["$secret"],
            "TOKEN"
        );
    }

    #[test]
    fn fleet_config_missing_mcp_servers_is_empty_with_projects() {
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
    fn bidi_capability_gate() {
        for p in [
            Provider::Glm,
            Provider::Deepseek,
            Provider::Minimax,
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
        let _guard = crate::test_env_lock();
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

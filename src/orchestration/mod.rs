pub mod account_probes;
pub mod agents;
pub mod allocator;
pub mod atoms;
pub mod badgey;
pub mod brofile;
pub mod capabilities;
pub mod http_fetch;
pub mod mcp;
pub mod providers;
pub mod resume_lease;
pub mod supervision;
pub mod tail;
pub mod team;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, OnceLock};

use parking_lot::{Mutex, RwLock};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
use tokio::process::Command;
use tokio::sync::Notify;

use crate::transcripts::adapters::TranscriptAdapterRegistry;
use crate::transcripts::types::{TranscriptCursor, TranscriptLocation};
use providers::dispatch_prelude::*;
use providers::{EventSink, Provider, Usage};
use supervision::SupervisionState;

const BLACKBOX_SERVICE_ENV_VARS: &[&str] = &[
    "BBOX_PORT",
    "BLACKBOX_MCP_NAME",
    "BLACKBOX_MCP_URL",
    "BLACKBOX_STATE_DIR",
    "BLACKBOX_KNOWLEDGE_PATH",
    "BLACKBOX_GAPS_PATH",
    "BLACKBOX_THREADS_PATH",
    "BLACKBOX_NOTES_PATH",
    "BLACKBOX_GLOBAL_CLAUDE_MD",
    "BLACKBOX_GLOBAL_CODEX_MD",
    "BLACKBOX_GLOBAL_GEMINI_MD",
    "BLACKBOX_BACKUP_DIR",
    "BRO_HOME",
    "TRANSCRIPT_SEARCH_ROOTS",
    "TRANSCRIPT_SEARCH_CODEX_ROOT",
    "TRANSCRIPT_SEARCH_INDEX_PATH",
];

const PROMPT_STDIN_ARG_BYTES_THRESHOLD: usize = 64 * 1024;

fn harness_controls()
-> &'static RwLock<HashMap<String, bro_harness::agent_loop::SessionInputSender>> {
    static CONTROLS: OnceLock<
        RwLock<HashMap<String, bro_harness::agent_loop::SessionInputSender>>,
    > = OnceLock::new();
    CONTROLS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Translate a `bro_protocol::SessionCommand` — the daemon↔client control-plane
/// contract (harness-daemon-boundary.md §8/§11) — into the harness's internal
/// `SessionInput` and deliver it to a live in-process session. The protocol enum
/// is the shared schema; this is the one place it crosses into harness-local
/// types. Every variant maps to a genuinely-handled harness path (no acked
/// no-ops): UserTurn→User, Interrupt→interrupt control, SetModel→set_model
/// control, Compact→the `/compact` in-stream slash command.
pub fn apply_session_command(
    task_id: &str,
    command: bro_protocol::SessionCommand,
) -> Result<(), String> {
    use bro_harness::agent_loop::SessionInput;
    use bro_protocol::SessionCommand;

    let tx = harness_controls()
        .read()
        .get(task_id)
        .cloned()
        .ok_or_else(|| format!("task {task_id} has no live in-process harness control channel"))?;

    let input = match command {
        SessionCommand::UserTurn { text } => SessionInput::User(text),
        SessionCommand::Interrupt => SessionInput::Control {
            subtype: "interrupt".to_string(),
            request_id: Some(uuid::Uuid::new_v4().to_string()),
            raw: serde_json::json!({"type": "control_request", "subtype": "interrupt"}),
        },
        SessionCommand::SetModel { model } => SessionInput::Control {
            subtype: "set_model".to_string(),
            request_id: Some(uuid::Uuid::new_v4().to_string()),
            raw: serde_json::json!({
                "type": "control_request",
                "subtype": "set_model",
                "model": model,
            }),
        },
        // `/compact` is an in-stream slash command, not a control_request.
        SessionCommand::Compact => SessionInput::User("/compact".to_string()),
    };

    tx.send(input)
        .map_err(|_| format!("task {task_id} harness control channel is closed"))
}

pub fn steer_harness_task(task_id: &str, prompt: String) -> Result<(), String> {
    apply_session_command(
        task_id,
        bro_protocol::SessionCommand::UserTurn { text: prompt },
    )
}

pub fn interrupt_harness_task(task_id: &str, redirect: Option<String>) -> Result<(), String> {
    match redirect {
        // Interrupt-and-redirect (§8 op 3): the prompt rides the interrupt
        // control's raw so the harness dequeues it immediately on cancel. This
        // payload shape has no SessionCommand variant, so it stays inline.
        Some(prompt) => {
            let tx = harness_controls()
                .read()
                .get(task_id)
                .cloned()
                .ok_or_else(|| {
                    format!("task {task_id} has no live in-process harness control channel")
                })?;
            tx.send(bro_harness::agent_loop::SessionInput::Control {
                subtype: "interrupt".to_string(),
                request_id: Some(uuid::Uuid::new_v4().to_string()),
                raw: serde_json::json!({
                    "type": "control_request",
                    "subtype": "interrupt",
                    "prompt": prompt,
                }),
            })
            .map_err(|_| format!("task {task_id} harness control channel is closed"))
        }
        None => apply_session_command(task_id, bro_protocol::SessionCommand::Interrupt),
    }
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BroReport {
    pub message: String,
    #[serde(default)]
    pub needs: Option<String>,
    #[serde(default)]
    pub data: Option<Value>,
    #[serde(rename = "reportedAt")]
    pub reported_at: u64,
}

impl BroReport {
    pub fn to_json(&self) -> Value {
        let mut obj = serde_json::json!({
            "message": self.message,
            "reportedAt": self.reported_at,
            "reportedAgo": format_elapsed(self.reported_at, None),
        });
        if let Some(ref needs) = self.needs {
            obj["needs"] = Value::String(needs.clone());
        }
        if let Some(ref data) = self.data {
            obj["data"] = data.clone();
        }
        obj
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
    /// Caller-supplied identity for the dispatched bro. Format:
    /// `<team>::<member>` for ensemble dispatch (carries which member
    /// of which team this task belongs to), bare `<brofile>` for
    /// brofile-only dispatch (no team context — implementer / advisor
    /// nodes), or `None` for legacy direct dispatches that didn't
    /// supply context. The tail handler reads this when team-based
    /// `find_bro_ref_for_task` returns no match, so brofile-dispatched
    /// tasks (workflow implementer, single-bro advisor) still surface
    /// in `bro tail` with a name instead of being anonymous.
    pub bro_label: Option<String>,
    /// Agent attribution set by bro_agent_dispatch. Format:
    /// `agent:<name>@v<version>`. Preserved even when record_task_to_bro
    /// overwrites bro_label for team routing. Surfaced in bro_status /
    /// bro_dashboard as agentLabel alongside broLabel.
    pub agent_label: Option<String>,
    /// Latest agent-authored progress report, set through `bro_report`
    /// and surfaced in `bro_status` / `bro_dashboard`.
    pub report: Option<BroReport>,
    /// True when this task's terminal state is "the daemon killed it
    /// because the process restarted, but the underlying provider
    /// session_id is still valid on disk (rollout / session jsonl
    /// persisted)." Calling agents that see `status=failed` AND
    /// `recoverable=true` should retry via `bro_resume(session_id=...)`
    /// rather than starting a fresh session — the conversation
    /// history is intact. Surfaced through `bro_status` / `bro_wait`.
    pub recoverable: bool,
    pub transcript_location: Option<TranscriptLocation>,
    pub transcript_cursor: Option<TranscriptCursor>,
    pub supervision: SupervisionState,
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

    /// PID of the spawned provider child while the task is running.
    /// `None` once the process has exited or `cancel_task` has taken
    /// the handle. Council uses this to poll for actual child exit
    /// after a SIGTERM, so the resume lease can be held until the
    /// session jsonl writer is truly gone.
    pub fn child_pid(&self) -> Option<u32> {
        *self.child_id.lock()
    }
}

/// Test-only Task constructor for store/prune unit tests. Private fields
/// (`child_id`) keep callers outside this module from building Tasks, so
/// this gated helper is the seam tests use to populate a TaskStore.
#[cfg(test)]
pub(crate) fn test_task(id: &str, status: TaskStatus, provider: Provider) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: id.into(),
            provider,
            session_id: format!("sess-{id}"),
            events: vec![],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: Some(3),
            stderr: String::new(),
            status,
            started_at: now_ms(),
            completed_at: Some(now_ms()),
            exit_code: Some(0),
            cwd: None,
            bro_label: None,
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

// ---------------------------------------------------------------------------
// Task Store
// ---------------------------------------------------------------------------

pub struct TaskStore {
    tasks: HashMap<String, Arc<Task>>,
    reserved: HashSet<String>,
}

impl TaskStore {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            reserved: HashSet::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.get(id).cloned()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tasks.contains_key(id) || self.reserved.contains(id)
    }

    pub fn reserve_id(&mut self, id: &str) -> Result<(), BroSpawnError> {
        if self.contains(id) {
            return Err(BroSpawnError::DuplicateTaskId { id: id.to_string() });
        }
        self.reserved.insert(id.to_string());
        Ok(())
    }

    #[allow(dead_code)] // test-only entry point; production paths use reserve_id + insert_reserved
    pub fn insert(&mut self, id: String, task: Arc<Task>) -> Result<(), BroSpawnError> {
        if self.tasks.contains_key(&id) {
            return Err(BroSpawnError::DuplicateTaskId { id });
        }
        if self.reserved.contains(&id) {
            return Err(BroSpawnError::ReservedTaskId { id });
        }
        self.tasks.insert(id, task);
        Ok(())
    }

    fn insert_reserved(&mut self, id: String, task: Arc<Task>) -> Result<(), BroSpawnError> {
        if self.tasks.contains_key(&id) {
            self.reserved.remove(&id);
            return Err(BroSpawnError::DuplicateTaskId { id });
        }
        self.reserved.remove(&id);
        self.tasks.insert(id, task);
        Ok(())
    }

    fn insert_loaded(&mut self, id: String, task: Arc<Task>) {
        self.reserved.remove(&id);
        self.tasks.entry(id).or_insert(task);
    }

    fn release_reservation(&mut self, id: &str) {
        self.reserved.remove(id);
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
    #[serde(default)]
    bro_label: Option<String>,
    #[serde(default)]
    agent_label: Option<String>,
    #[serde(default)]
    report: Option<BroReport>,
    /// True when the previous daemon instance was running this task
    /// at restart and the underlying provider session_id is still
    /// recoverable on disk. Set during `TaskStore::load` when it
    /// flips a `Running` task to `Failed`. Lets calling agents
    /// distinguish "kill by restart, rollout intact, retry via
    /// `bro_resume(session_id=...)`" from "task genuinely failed".
    #[serde(default)]
    recoverable: bool,
    #[serde(default)]
    transcript_location: Option<TranscriptLocation>,
    #[serde(default)]
    transcript_cursor: Option<TranscriptCursor>,
    #[serde(default)]
    supervision: SupervisionState,
}

impl TaskStore {
    pub fn persist(&self, store_dir: &std::path::Path) {
        self.persist_with_event_limit(store_dir, MAX_PERSISTED_EVENTS);
    }

    /// Full-history persist (no per-task event cap). The fleet client was the
    /// only production caller; with the §7 fleet-daemon-only cut its own mirror
    /// store owns this now (`bro-fleet-client`). Retained (allow-dead) for the
    /// persistence-contract test and any future full-history daemon path.
    #[allow(dead_code)]
    pub fn persist_all_events(&self, store_dir: &std::path::Path) {
        self.persist_with_event_limit(store_dir, usize::MAX);
    }

    fn persist_with_event_limit(&self, store_dir: &std::path::Path, event_limit: usize) {
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
                        .take(event_limit)
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
                    bro_label: inner.bro_label.clone(),
                    agent_label: inner.agent_label.clone(),
                    report: inner.report.clone(),
                    recoverable: inner.recoverable,
                    transcript_location: inner.transcript_location.clone(),
                    transcript_cursor: inner.transcript_cursor.clone(),
                    supervision: inner.supervision.clone(),
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
                rec.stderr.push_str(
                    "\n[blackbox] server restarted while task was running. \
                     The provider session is still on disk; retry with \
                     `bro_resume(session_id=...)` to continue the conversation \
                     rather than starting a fresh session.",
                );
                rec.recoverable = true;
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
                    bro_label: rec.bro_label,
                    agent_label: rec.agent_label,
                    report: rec.report,
                    recoverable: rec.recoverable,
                    transcript_location: rec.transcript_location,
                    transcript_cursor: rec.transcript_cursor,
                    supervision: rec.supervision,
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            store.insert_loaded(rec.id, task);
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
/// because it rides with every turn. Keep the wording calibrated as
/// a knowledge/runbook recall check, not a mandatory first action, because Codex tends
/// to over-comply with hard ordering language here. Parallels the
/// empirical fix for `bbox_note` emission (DEFAULT_COMPLETION_CONTRACT
/// below).
pub const RECALL_DIRECTIVE: &str = "\
Recall: early in tasks where durable knowledge, prior decisions, conventions, \
or system runbooks could change the answer, query `bbox_knowledge` with a \
short phrase from the user's request. It is not the surface for scoped pins, \
side-channel notes, active threads, or transcripts. If the result is empty or \
too broad, try one sharper phrase before relying on live filesystem state or \
prior knowledge.";

/// Ambient nudge for recursive orchestrators (allow_recursion=true).
/// They're usually fan-out coordinators, and the most common silent
/// failure mode is writing a prose rubric and pasting it into N
/// identical sub-agent prompts instead of compiling a packet once and
/// dispatching the packet_id. Fires in addition to RECALL_DIRECTIVE.
pub const ORCHESTRATOR_HINT: &str = "\
Orchestrator note: if your plan involves sending the same rubric / \
ranking criteria / decision tree / access rules to multiple sub-agents, \
compile it into a packet first via `bbox_compile` and dispatch the \
`packet_id` — every sub-agent then produces bit-identical output via \
`bbox_apply`, and a 4th agent can reproduce the results deterministically \
without re-reading prose. See `sm-rule-packets` via `bbox_knowledge`.";

/// Ambient task-shape nudge for every dispatch. Addresses a failure
/// mode observed in E10/S11 where an agent bypassed packets entirely
/// on a log-triage task ("the primitive was simply absent from my
/// mental toolkit") because `bbox_compile` wasn't deferred-loaded and
/// the task's shape read as regex-ish. Naming the packet tools in
/// the ambient prefix pre-loads their schemas into the agent's option
/// space and reframes "the AST can't do regex" as a gap-log rather
/// than a bypass. Fires for every dispatch, orthogonal to
/// ORCHESTRATOR_HINT.
///
/// **Calibration bound (E12 cross-provider data):** the current
/// wording is at the "works across claude+codex+gemini" joint.
/// Self-reported force varies by provider — Claude reads it as
/// "nudge, not decider" ("I'd have compiled regardless"), Codex as
/// "could have tipped fuzzier tasks", Gemini as "MANDATORY choice,
/// not nudge". Escalating the language (e.g. imperative verbs,
/// longer justification, explicit step-by-step) risks:
///   (a) over-constraint on Claude — it becomes noise it ignores,
///       or worse, makes the hint feel adversarial in tasks where
///       packets are clearly wrong (prose/research/synthesis);
///   (b) compliance theater on Gemini — Gemini already treats this
///       as mandatory at current wording; turning the dial up could
///       make it compile packets for tasks where the AST doesn't
///       fit, defeating the gap-tool signal.
///
/// If you change this string, re-run E12 (cross-provider S11 sweep)
/// to confirm all three providers still read it as intended.
/// Don't add imperative verbs ("MUST compile", "ALWAYS use") without
/// that verification.
pub const TASK_SHAPE_HINT: &str = "\
Task-shape check: if this task involves repeatedly classifying, \
ranking, triaging, scoring, or judging entities against stated \
criteria — try `bbox_compile` first (see `sm-rule-packets` via \
`bbox_knowledge`). Packets force explicit rule ordering and buy a \
free audit via `bbox_audit`. Log a gap via `bbox_packet_gap` when \
the AST can't express what you need — whether mechanically (no \
operator fires) or conceptually (a composition works but is \
semantically blunt — e.g., keyword StringContains where you'd \
have wanted regex or synonym matching, Any{} over a long needle \
list where you wanted a generalizer). Fidelity 1.0 on training \
alone doesn't rule out the gap; if the mechanism won't generalize \
to unseen vocabulary, that's the signal the log wants.";

/// Default per-dispatch contract. Deliberately quiet: `bbox_note` is a
/// signal channel for *notable* observations, NOT a per-dispatch ritual, so
/// the baseline neither requires a `done` note nor demands a note per finding.
/// Callers that genuinely need a guaranteed structured sign-off (atoms,
/// workflows, badgey, review arcs) layer a stricter contract on top of this.
/// The only hard part retained is the task_id/scope correlation guidance —
/// when a note *is* emitted it must carry the right keys to land.
pub const DEFAULT_COMPLETION_CONTRACT: &str = "\
If something genuinely notable came up during the work, surface it with a \
`bbox_note` call so the orchestrator doesn't have to re-read \
your transcript. This is a signal channel, not a progress log — if nothing was \
notable, emit nothing. Use the kind that fits:\n\
   • `surprise` — you expected X and found Y.\n\
   • `dispute` — the brief is wrong or contradicts what you found.\n\
   • `blocked` — you could not proceed and why.\n\
   • `followup` — concrete out-of-scope work you noticed (record only; do not \
do it).\n\
   • `assumption` — an ambiguity-resolving judgment a reviewer should know about.\n\
   • `learned` — a reusable project-local fact worth keeping.\n\
A `kind=done` note with a specific one-line acceptance summary (\"verified X \
already handles Y\"; not \"task complete\") is worth emitting when a concise \
result would help the orchestrator act — but it is optional unless this \
dispatch's instructions require one.\n\
\n\
When you do emit a note, include the correlation keys so it lands:\n\
  task_id=<copy `task:` from [scope] above EXACTLY — not the project path, not \
prose, not \"pending\">\n\
  project=<`project` from [scope], if present>\n\
  bro=<`bro` from [scope], if present>\n\
  session_id=<`session` from [scope], if present>";

/// Workspace-tools appendix injected when `AmbientContext::coerce_workspace`
/// is true. Teaches agents to prefer workspace-scoped tool surfaces over
/// raw filesystem access. References the implemented workspace tool surface
/// (`work_smart_read`, `work_bash`, `work_git_*`) and the safe fallback
/// (`bbox_note(kind=learned)`) when those tools are not available in a
/// narrowed tool catalog.
pub const WORKSPACE_TOOLS_APPENDIX: &str = "\
[workspace-tools mode]\n\
You are in workspace-tools mode. Prefer workspace-scoped tool surfaces over \
raw filesystem access:\n\
  - Treat injected project docs/tool guidance as already-present context, not \
as a separate read-instructions ceremony. Open source files only when the next \
step needs exact lines, the injected copy appears stale, or the file was not \
injected.\n\
  - Start with the agentic grounding sequence: accept injected context and scope; \
run sandbox grounding; use blackbox retrieval/evidence bundling when claims \
depend on prior decisions, design docs, threads, code graph facts, or \
conversation history; then edit/validate in the grounded cwd.\n\
  - For the sandbox boundary, call `sandbox_grounding` when available: \
`sandbox_grounding(enter_worktree=false)` for read-only orientation, or \
`sandbox_grounding(enter_worktree=true, purpose=<short reason>)` before work \
that may edit files. It returns the launch manifest and, when a worktree is \
entered, the managed worktree plus `sandbox_status(root=<worktree cwd>)`. If \
that tool is unavailable, fall back to manual `sandbox_status`, `enter_worktree`, \
then `sandbox_status(root=<returned cwd>)`.\n\
  - For blackbox evidence, use the opening sequence instead of memory when \
provenance matters: `bbox_describe_schema` once per session, \
`bbox_hybrid_search`, `bbox_inspect_entity`, conditional `bbox_find_paths`, then \
`bbox_bundle_evidence` before making provenance-sensitive claims. The detailed \
question-shape runbook is `sm-agentic-opening-sequence`; pull it only when the \
injected tool guidance is insufficient. Use `property_mode=\"summary\"` when \
bundling broad tool/knowledge refs or other long entities. For fresh \
probe/retro evidence, if \
hybrid search returns only generic seeds, no results, or a degraded \
BM25-only/vector-warming notice, pivot to `bbox_notes`/`bbox_gaps` with exact \
task, project, bro, or short substrings before broadening to git/filesystem \
evidence. If `bbox_describe_schema` reports `project_file` / `project_file_v2` \
population `0`, do not investigate or patch indexing as part of a sandbox \
probe. State the corpus gap, dedupe/file a `sandbox-observability` gap if one \
does not already exist, and use `work_smart_read` or scoped file reads for exact \
code locations while still bundling any non-code bbox evidence that resolved \
cleanly.\n\
  - For authorial work, pick the matching primitive: `narf_exec` for one-shot JS \
cells; `narf_prepare` then `narf_run` when source/contract review matters; \
`bbox_refactor_status` then `bbox_refactor_plan` for guarded refactor planning; \
`bro_exec` then `bro_wait`/`bro_status`/`bro_resume` for ad-hoc child agents. \
NARF cells receive values, not ref envelopes; host tools return values; \
`narf.encode.yaml`, `narf.encode.frontmatter`, and `narf.encode.mdTable` cover \
non-JS-native formats (`mdTable(rows, columns?)` accepts object rows plus \
optional column order); `narf.kv.*` is exact-name deref only and does not \
enumerate in-box. Use `bbox_refactor_plan_kinds` after status inventory to pick \
a safe planning kind. If a child bro completes with an empty/suspicious result, \
or an LSP-backed refactor plan stays `tool_running` after a wait timeout, call \
`bro_status(tail=N)` before resuming, cancelling, or filing a gap.\n\
  - Prefer `work_smart_read` over `Read` for file inspection.\n\
  - Prefer `work_bash` over `Bash` for shell commands.\n\
  - Prefer `work_git_status` / `work_git_diff` / `work_git_log` over \
bare `Bash(\"git …\")` invocations.\n\
After `enter_worktree`, treat the returned `cwd` as authoritative. Generic \
file tools may still be rooted at the original checkout; prefer work_* tools \
or pass absolute paths under the returned worktree.\n\
When a workspace tool is not available in the current session, fall back to \
the standard tool and emit `bbox_note(kind=learned, body=\"work_* unavailable, \
used <standard_tool> as fallback\")` so the orchestrator can track coverage.\n\
Do NOT implement `work_*` handlers yourself; they are provided by the host.\n\
Do NOT add new workspace tool names under the `bbox_*` namespace.";

/// The workload-retrospective probe prompt, injected as a fake user turn
/// when a bro's own session is resumed by `bro_prune(retro=true)` or
/// `bro_retro`. It invites — but never compels — a `bbox_gap` substrate-gap
/// report about friction with the *blackbox substrate itself*.
///
/// The wording is the policy: there is no rule engine deciding what's
/// worth filing, so the prompt alone has to calibrate the bro's judgment
/// without skewing it. Two failure modes it steers between:
///   - **Compulsion** (a quota or "silence = incomplete" framing) →
///     manufactured friction, inbox noise.
///   - **Discouragement** ("only if it's a big deal") → real gaps go
///     unreported.
/// Levers: both file/don't-file outcomes are explicitly blameless; the
/// bar is concreteness/utility, not count; all evaluative stakes are
/// stripped; the anti-pattern is named out loud. A hard scope boundary
/// keeps it to surfaces blackbox can actually change (its MCP tools,
/// guidance, orchestration) and explicitly waves off the target repo,
/// language toolchain, and external services — those would be noise.
///
/// Field names and the `gap_kind` enum are pinned to `gaps.rs` (`GapKind`);
/// an unknown gap_kind or malformed dedupe_key would fail `bbox_gap`.
/// Deliberately NOT routed through `apply_ambient` — its recall /
/// task-shape nudges miscue a reflection turn (see `workload_retro_prompt`).
pub const WORKLOAD_RETRO_PROMPT: &str = "\
Quick retrospective — the task itself is done, nothing more is needed on it.\n\
\n\
While it's still fresh, give a short sandbox-focused assessment of the run. \
Think only about the blackbox tooling you worked through — the bbox_/bro_/work_ \
MCP tools, the sandbox/worktree boundary, the guidance and memories blackbox \
gave you, and the workflow or dispatch path it ran you through.\n\
\n\
First answer these sandbox questions in prose, even if the answers are boring:\n\
  • Did you know which cwd/base repo/managed worktree you were operating in, \
and which roots were writable?\n\
  • Could you tell which project docs, provider/account/session env, and MCP \
surface shaped the sandbox? Did `sandbox_status` or an equivalent manifest make \
that easy?\n\
  • When the task depended on prior decisions, design docs, threads, code graph \
facts, or history, was the blackbox opening sequence and `bbox_bundle_evidence` \
path clear enough to ground claims? If you skipped it, was that because the task \
did not need provenance or because the path was awkward?\n\
  • Could an outside observer reconstruct the important file reads/writes, \
shell commands, denials, cwd changes, env overrides, and tool calls from the \
available output?\n\
  • Did the sandbox encourage native idioms such as `note()`/`hybrid_search()`/\
`smart_read()`/`work_bash()`, or did you have to translate through awkward \
outside-daemon forms?\n\
\n\
Then, only if something concrete stands out, consider filing gaps for:\n\
  • a bbox_/bro_/work_ tool you reached for that didn't exist, or one that \
existed but fought you — missing parameter, awkward output, wrong shape;\n\
  • sandbox grounding that was missing or hard to trust — cwd/base repo/managed \
worktree, writable roots, durable project scope, provider/session env, MCP \
surface, denied paths, file writes, or shell commands weren't visible enough;\n\
  • a sandbox-native idiom that would have been clearer than the outside-daemon \
form you had to use, e.g. `note()`/`hybrid_search()`/`smart_read()`/`work_bash()` \
instead of a fully-qualified MCP/tool spelling;\n\
  • an evidence-bundling or blackbox-opening-sequence step that was unclear, \
too easy to skip, or awkward to complete before making a provenance-sensitive \
claim;\n\
  • something blackbox told you — a system memory, a rendered convention, a \
runbook — that was missing, stale, or actively misleading;\n\
  • a blackbox workflow or dispatch step that was clumsier than it should be.\n\
\n\
Scope matters: this channel is only for things blackbox itself can change — its \
own tools, guidance, and orchestration. It is NOT for the project you were \
working on, its compiler or language toolchain, or external services. If what \
fought you was rustc, a flaky API, or a gap in the target repo's own docs — \
that's real, but it's out of scope here, so skip it.\n\
\n\
If something concrete and in-scope stands out — something a future agent or the \
operator would genuinely be glad blackbox knew — file one gap per distinct gap \
with bbox_gap:\n\
  • title: one-line summary  (required)\n\
  • gap_kind: one of mcp_surface, tooling, workflow, agent, docs_runbook, \
refactor_primitive, ontology, eval_coverage  (required) — mcp_surface for a \
missing/awkward bbox_/bro_/work_ tool or sandbox-native alias, tooling for a \
missing sandbox observation primitive, docs_runbook for missing/stale guidance \
or memory, workflow for dispatch/orchestration friction;\n\
  • domain: the blackbox subsystem it touches, e.g. orchestration, knowledge, \
transcripts, refactor, harness/sandbox  (required)\n\
  • wanted_capability: what you wished existed, concretely  (required)\n\
  • dedupe_key as \"<gap_kind>/<domain>/<slug>\" so duplicates from other runs \
collapse, e.g. \"mcp_surface/transcripts/regex-search\"  (required)\n\
  • optional but helpful: missing_primitive; impact (low|medium|high|critical); \
fallback_used; notes.\n\
Run bbox_gaps first to dedupe — an open gap with the same dedupe_key collapses \
automatically. Keep each gap specific enough to act on.\n\
\n\
If nothing in-scope stands out, that's a completely normal way for a run to \
end — say so and file nothing. No quota, no expectation; a quiet run is a good \
run. File only when you'd genuinely want someone to see it, and please don't \
manufacture friction just to have something to say.\n\
\n\
Return this shape:\n\
Sandbox assessment: clear / mixed / unclear — one sentence.\n\
Observation surface: what was visible enough, and the biggest missing surface \
if any.\n\
Friction: none, nitpick, wishlist, or actionable gap filed.\n\
Gaps filed: list dedupe keys, or `none`.";

/// Build the workload-retro probe prompt with a minimal `[scope]` block so
/// any gap note the bro files carries the session/project correlation keys
/// and lands in `bbox_inbox` attributed correctly. Deliberately bypasses
/// `apply_ambient`: the recall directive and task-shape (packet) nudge it
/// injects would miscue a reflection turn, and the retro prompt already
/// names the exact `bbox_note` call it wants.
pub fn workload_retro_prompt(session_id: &str, project: Option<&str>) -> String {
    let mut scope = format!("[scope] session:{session_id}");
    if let Some(p) = project {
        scope.push_str(" · project:");
        scope.push_str(p);
    }
    format!("{scope}\n\n{WORKLOAD_RETRO_PROMPT}")
}

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
    /// Scoped active-arc guidance injected from bbox_pin. Persisted on disk,
    /// hot only for matching ambient scopes, never rendered into repo memory.
    pub pin_block: Option<String>,
    /// Per-dispatch expectation, e.g. "call bbox_note(kind='done', body='…') before returning".
    pub completion_contract: Option<String>,
    pub allow_recursion: bool,
    /// Target provider. When set and the provider supports dispatch-time
    /// tool filtering (Claude/Copilot), the text recursion guard is
    /// omitted in favor of the mechanical filter applied at the CLI arg
    /// layer. Unset or unsupported provider → text guard as fallback.
    #[allow(dead_code)]
    // reserved hook for defense-in-depth text guards; see comment above apply_ambient
    pub provider: Option<providers::Provider>,
    /// Inject workspace-tools appendix. When true, `apply_ambient` appends
    /// the WORKSPACE_TOOLS_APPENDIX after the completion contract, teaching
    /// the agent to prefer work_smart_read / work_bash / work_git_* over
    /// raw filesystem access. Sourced from brofile `coerce_workspace` or
    /// per-dispatch ExecParams/ResumeParams override. Default off.
    pub coerce_workspace: bool,
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
/// recall directive + optional orchestrator hint + optional completion
/// contract). Does NOT touch the brofile lens.
///
/// Recursion guarding (blocking sub-bro dispatch) is done mechanically
/// via provider-specific tool-filter args (`--disallowedTools`,
/// `--deny-tool`, `-c disabled_tools=…`, or `--policy <file>`), appended
/// to argv outside this function. No text recursion guard is emitted.
///
/// The ambient prefix fires for every dispatch regardless of
/// `allow_recursion`: the scope block lets the agent correlate notes,
/// the recall directive prompts context lookup when relevant, and the
/// orchestrator hint surfaces packet primitives for fan-out coordinators.
/// These are purely textual — they don't interact with the mechanical
/// recursion filter.
pub fn apply_ambient(prompt: &str, ctx: &AmbientContext) -> String {
    let mut prefix = String::new();

    let fields = ctx.scope_fields();
    if !fields.is_empty() {
        prefix.push_str("[scope] ");
        prefix.push_str(&fields.join(" · "));
        prefix.push_str("\n\n");
    }

    if let Some(pin_block) = &ctx.pin_block {
        prefix.push_str("[scoped pins]\n");
        prefix.push_str(pin_block.trim_end());
        prefix.push_str("\n\n");
    }

    // Per-turn recall reinforcement. Session-start memory guidance
    // decays at depth on Claude and Gemini; ambient survives because
    // it rides with every turn.
    prefix.push_str("[recall before acting]\n");
    prefix.push_str(RECALL_DIRECTIVE);
    prefix.push_str("\n\n");

    // Packet-primitive awareness nudge for every dispatch. Addresses
    // the S11-shape silent-bypass where the agent doesn't have
    // `bbox_compile` in their mental toolkit when a structured-task
    // prompt arrives. Puts it there before plan formation.
    prefix.push_str("[task shape]\n");
    prefix.push_str(TASK_SHAPE_HINT);
    prefix.push_str("\n\n");

    // When the caller explicitly enabled recursion, this agent is a
    // fan-out orchestrator. Surface the packet primitive — the most
    // common silent miss for these agents is writing a prose rubric
    // and pasting it into N identical sub-agent prompts.
    if ctx.allow_recursion {
        prefix.push_str("[orchestrator]\n");
        prefix.push_str(ORCHESTRATOR_HINT);
        prefix.push_str("\n\n");
    }

    if let Some(contract) = &ctx.completion_contract {
        prefix.push_str("[completion contract]\n");
        prefix.push_str(contract.trim_end());
        prefix.push_str("\n\n");
    }

    if ctx.coerce_workspace {
        prefix.push_str(WORKSPACE_TOOLS_APPENDIX);
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BroSpawnError {
    DuplicateTaskId {
        id: String,
    },
    #[allow(dead_code)] // only constructed by the test-only `insert` method
    ReservedTaskId {
        id: String,
    },
}

impl std::fmt::Display for BroSpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DuplicateTaskId { id } => write!(f, "duplicate task id: {id}"),
            Self::ReservedTaskId { id } => write!(f, "task id is already reserved: {id}"),
        }
    }
}

impl std::error::Error for BroSpawnError {}

pub struct SpawnTaskParams {
    pub provider: Provider,
    pub args: Vec<String>,
    /// Provider session id to record. Harness-backed fresh dispatch normally
    /// pre-mints this in the daemon and passes it to the harness; legacy or
    /// provider-discovered paths may still use a temporary placeholder such as
    /// `pending` until the first stream event.
    pub session_id: String,
    pub cwd: Option<String>,
    pub env_overrides: Option<HashMap<String, String>>,
    pub store_dir: std::path::PathBuf,
    pub task_store: Arc<RwLock<TaskStore>>,
    pub tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    pub bro_label: Option<String>,
    pub agent_label: Option<String>,
    /// System event hub for emitting task lifecycle events. Task events
    /// are observation-only: emit failures are logged but do not affect
    /// task dispatch.
    pub system_events: Option<crate::system_events::SharedEventHub>,
    /// Persistent bidirectional session mode (fleet-tui.md item 6). When true,
    /// child stdin is piped and kept **open and writable** after spawn (returned
    /// on [`SpawnedTask::stdin`]) instead of being closed after the one-shot
    /// prompt, so the caller can drive successive user-turns and
    /// `control_request`s. One-shot dispatch leaves this false.
    pub interactive: bool,
}

/// Result of a spawn: the tracked task plus, in `interactive` mode, the writable
/// child stdin for driving a persistent bidirectional session. `stdin` is `None`
/// in one-shot mode (closed after the prompt) and on spawn failure.
pub struct SpawnedTask {
    pub task: Arc<Task>,
    // Read only by the (removed) fleet in-process interactive launch; kept with
    // `spawn_task_interactive` until the daemon-side dispatch is consolidated.
    #[allow(dead_code)]
    pub stdin: Option<tokio::process::ChildStdin>,
}

pub fn spawn_with_pre_minted_id(
    task_id: String,
    params: SpawnTaskParams,
) -> Result<Arc<Task>, BroSpawnError> {
    params.task_store.write().reserve_id(&task_id)?;
    Ok(spawn_task_reserved(task_id, params).task)
}

fn failed_duplicate_task(
    task_id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    message: String,
) -> Arc<Task> {
    Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task_id,
            provider,
            session_id,
            events: vec![],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: message,
            status: TaskStatus::Failed,
            started_at: now_ms(),
            completed_at: Some(now_ms()),
            exit_code: None,
            cwd,
            bro_label,
            agent_label,
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

/// Create a tracked task for daemon-internal async work. This mirrors
/// provider-backed tasks closely enough that `bro_status`, `bro_wait`,
/// dashboards, persistence, and tail subscribers can observe it.
pub fn spawn_in_process_task(
    task_id: String,
    provider: Provider,
    session_id: String,
    cwd: Option<String>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    system_events: Option<crate::system_events::SharedEventHub>,
) -> Arc<Task> {
    if let Err(err) = task_store.write().reserve_id(&task_id) {
        if let Some(existing) = task_store.read().get(&task_id) {
            return existing;
        }
        let failed = failed_duplicate_task(
            task_id,
            provider,
            session_id,
            cwd,
            bro_label,
            agent_label,
            err.to_string(),
        );
        failed.notify.notify_waiters();
        return failed;
    }

    let task = Arc::new(Task {
        inner: Mutex::new(TaskInner {
            id: task_id.clone(),
            provider,
            session_id: session_id.clone(),
            events: Vec::new(),
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: now_ms(),
            completed_at: None,
            exit_code: None,
            cwd,
            bro_label,
            agent_label,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(None),
    });

    if let Err(err) = task_store
        .write()
        .insert_reserved(task_id.clone(), task.clone())
    {
        task_store.write().release_reservation(&task_id);
        let failed = failed_duplicate_task(
            task_id,
            provider,
            session_id,
            task.inner.lock().cwd.clone(),
            None,
            None,
            err.to_string(),
        );
        failed.notify.notify_waiters();
        return failed;
    }
    task_store.read().persist(&store_dir);
    let task_id_ev = task_id.clone();
    let bro_ev = task.inner.lock().bro_label.clone();
    let provider_str = provider.to_string();
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        task_id,
        provider,
        bro_name: None,
    });
    // Emit task.started system event. Observation-only: failures logged, not propagated.
    if let Some(hub) = system_events {
        tokio::spawn(async move {
            let mut correlation = serde_json::Map::new();
            correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
            let draft = crate::system_events::SystemEventDraft {
                kind: crate::system_events::types::SystemEventKind::TaskStarted,
                producer: "orchestration.dispatch".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation,
                causation_id: None,
                payload: serde_json::json!({
                    "task_id": task_id_ev,
                    "provider": provider_str,
                    "bro": bro_ev,
                }),
            };
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task.started (in-process) system event emit failed: {e:#}");
            }
        });
    }
    task
}

pub fn push_in_process_event(task: &Task, event: Value) {
    let mut inner = task.inner.lock();
    inner.events.push(event);
}

pub fn finish_in_process_task(
    task: &Task,
    status: TaskStatus,
    result: Option<String>,
    stderr: Option<String>,
    task_store: &RwLock<TaskStore>,
    store_dir: &std::path::Path,
    tail_tx: &tokio::sync::broadcast::Sender<tail::TailEvent>,
    system_events: Option<crate::system_events::SharedEventHub>,
) {
    let mut inner = task.inner.lock();
    if let Some(result) = result {
        inner.last_assistant_message = Some(result);
    }
    if let Some(stderr) = stderr {
        inner.stderr.push_str(&stderr);
    }
    inner.status = status;
    inner.completed_at = Some(now_ms());
    let task_id = inner.id.clone();
    let elapsed = format_elapsed(inner.started_at, inner.completed_at);
    let cost = inner.cost_usd;
    let source_session = inner.session_id.clone();
    let task_kind = inner.bro_label.clone();
    let error: String = inner.stderr.chars().take(200).collect();
    drop(inner);

    match status {
        TaskStatus::Completed => {
            let _ = tail_tx.send(tail::TailEvent::TaskCompleted {
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
                cost,
                source_session,
                task_kind,
            });
        }
        TaskStatus::Failed => {
            let _ = tail_tx.send(tail::TailEvent::TaskFailed {
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
                error: error.clone(),
            });
        }
        TaskStatus::Cancelled => {
            let _ = tail_tx.send(tail::TailEvent::TaskCancelled {
                task_id: task_id.clone(),
                elapsed: elapsed.clone(),
            });
        }
        TaskStatus::Running => {}
    }
    // Emit terminal system event. Observation-only: failures logged, not propagated.
    if let Some(hub) = system_events {
        let task_id_ev = task_id.clone();
        let elapsed_ev = elapsed.clone();
        let (kind, payload) = match status {
            TaskStatus::Completed => (
                crate::system_events::types::SystemEventKind::TaskCompleted,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev, "cost_usd": cost}),
            ),
            TaskStatus::Failed => (
                crate::system_events::types::SystemEventKind::TaskFailed,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev, "error": error}),
            ),
            TaskStatus::Cancelled => (
                crate::system_events::types::SystemEventKind::TaskCancelled,
                serde_json::json!({"task_id": task_id_ev, "elapsed": elapsed_ev}),
            ),
            TaskStatus::Running => {
                // No terminal event for running state.
                task_store.read().persist(store_dir);
                task.notify.notify_waiters();
                return;
            }
        };
        let mut correlation = serde_json::Map::new();
        correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
        let draft = crate::system_events::SystemEventDraft {
            kind,
            producer: "orchestration.dispatch".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload,
        };
        tokio::spawn(async move {
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task terminal (in-process) system event emit failed: {e:#}");
            }
        });
    }
    task_store.read().persist(store_dir);
    task.notify.notify_waiters();
}

/// Emit a `task.progress` system event for one deduplicated snippet.
/// Spawns a background task; observation-only — failures are logged but do not
/// affect streaming.
pub(crate) fn emit_task_progress_event(
    hub: &crate::system_events::SharedEventHub,
    task_id: String,
    activity: String,
) {
    let hub = hub.clone();
    tokio::spawn(async move {
        let mut correlation = serde_json::Map::new();
        correlation.insert("task_id".into(), serde_json::json!(task_id));
        let draft = crate::system_events::SystemEventDraft {
            kind: crate::system_events::types::SystemEventKind::TaskProgress,
            producer: "orchestration.dispatch".to_string(),
            project: None,
            principal: None,
            subject: None,
            correlation,
            causation_id: None,
            payload: serde_json::json!({
                "task_id": task_id,
                "activity": activity,
            }),
        };
        if let Err(e) = hub.emit(draft).await {
            tracing::warn!("task.progress system event emit failed: {e:#}");
        }
    });
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
    bro_label: Option<String>,
    agent_label: Option<String>,
    system_events: Option<crate::system_events::SharedEventHub>,
) -> Arc<Task> {
    spawn_task_with_tool_placement(
        task_id,
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        bro_label,
        agent_label,
        None,
        system_events,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_task_with_tool_placement(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    tool_placement: Option<BTreeMap<String, String>>,
    system_events: Option<crate::system_events::SharedEventHub>,
) -> Arc<Task> {
    if matches!(
        provider,
        Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Brodex
            | Provider::VibeBh
    ) {
        return spawn_harness_in_process_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            task_store,
            tail_tx,
            bro_label,
            agent_label,
            tool_placement,
            system_events,
        );
    }

    if let Err(err) = task_store.write().reserve_id(&task_id) {
        if let Some(existing) = task_store.read().get(&task_id) {
            return existing;
        }
        return failed_duplicate_task(
            task_id,
            provider,
            session_id,
            cwd,
            bro_label,
            agent_label,
            err.to_string(),
        );
    }

    let params = SpawnTaskParams {
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        bro_label,
        agent_label,
        system_events,
        interactive: false,
    };

    spawn_task_reserved(task_id, params).task
}

#[allow(clippy::too_many_arguments)]
fn spawn_harness_in_process_task(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    tool_placement: Option<BTreeMap<String, String>>,
    system_events: Option<crate::system_events::SharedEventHub>,
) -> Arc<Task> {
    let task = spawn_in_process_task(
        task_id.clone(),
        provider,
        session_id,
        cwd.clone(),
        store_dir.clone(),
        task_store.clone(),
        tail_tx.clone(),
        bro_label,
        agent_label,
        system_events.clone(),
    );
    if task.inner.lock().status != TaskStatus::Running {
        return task;
    }

    let task_for_run = task.clone();
    let task_for_events = task.clone();
    let task_store_for_run = task_store.clone();
    let store_dir_for_run = store_dir.clone();
    let tail_for_events = tail_tx.clone();
    let tail_for_finish = tail_tx.clone();
    let system_events_for_events = system_events.clone();
    let system_events_for_finish = system_events.clone();
    let event_provider = provider;
    let event_task_id = task_id.clone();
    let (control_tx, control_rx) = bro_harness::agent_loop::session_input_channel();
    harness_controls()
        .write()
        .insert(task_id.clone(), control_tx);
    let callback = Arc::new(move |evt: Value| {
        ingest_harness_event(
            &task_for_events,
            event_provider,
            evt,
            &tail_for_events,
            &event_task_id,
            system_events_for_events.clone(),
        );
    });

    tokio::spawn(async move {
        let mut args = args;
        let result = async {
            let mcp_config = build_in_process_mcp_config(&mut args, tool_placement)?;
            run_harness_in_process(args, cwd, env_overrides, callback, control_rx, mcp_config).await
        }
        .await;
        let (mut status, stderr) = match result {
            Ok(()) => (TaskStatus::Completed, None),
            Err(err) => (TaskStatus::Failed, Some(format!("{err:#}\n"))),
        };
        harness_controls().write().remove(&task_id);
        {
            let inner = task_for_run.inner.lock();
            if matches!(inner.status, TaskStatus::Failed | TaskStatus::Cancelled) {
                status = inner.status;
            }
        }
        finish_in_process_task(
            &task_for_run,
            status,
            None,
            stderr,
            task_store_for_run.as_ref(),
            &store_dir_for_run,
            &tail_for_finish,
            system_events_for_finish,
        );
    });

    task
}

fn ingest_harness_event(
    task: &Task,
    provider: Provider,
    evt: Value,
    tail_tx: &tokio::sync::broadcast::Sender<tail::TailEvent>,
    task_id: &str,
    system_events: Option<crate::system_events::SharedEventHub>,
) {
    let snippet_to_emit = {
        let mut inner = task.inner.lock();
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
        let emitted_session_id = sink.session_id.clone();
        let mut accepted = true;
        let mut session_id_observed = false;
        if let Some(sid) = emitted_session_id {
            if inner.session_id == "pending" {
                inner.session_id = sid;
                session_id_observed = true;
            } else if inner.session_id != sid {
                reject_forked_session(&mut inner, &sid);
                accepted = false;
            }
        }
        if accepted {
            apply_cwd_updates_from_event(&mut inner, &evt);
            inner.supervision.observe_event(&evt, &sink, now_ms());
            apply_sink_updates(&mut inner, sink);
            // A terminal `result` event with `is_error: true` fails the task and
            // preserves the message in stderr. The subprocess path derives this
            // from a non-zero exit code, but the in-process harness loop returns
            // Ok regardless of turn outcome, so without this an errored turn
            // would be recorded as a silent successful completion (gap-32113fd4).
            if evt.get("type").and_then(Value::as_str) == Some("result")
                && evt.get("is_error").and_then(Value::as_bool) == Some(true)
            {
                if inner.status != TaskStatus::Cancelled {
                    inner.status = TaskStatus::Failed;
                }
                if let Some(msg) = evt.get("result").and_then(Value::as_str)
                    && !msg.trim().is_empty()
                {
                    inner.stderr.push_str(msg);
                    inner.stderr.push('\n');
                }
            }
        }
        accepted
            .then(|| {
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
            })
            .flatten()
            .map(|snippet| (snippet, session_id_observed))
            .or_else(|| session_id_observed.then(|| (String::new(), true)))
    };

    if let Some((snippet, session_id_observed)) = snippet_to_emit {
        if session_id_observed {
            task.notify.notify_waiters();
        }
        if snippet.is_empty() {
            return;
        }
        let _ = tail_tx.send(tail::TailEvent::TaskProgress {
            task_id: task_id.to_string(),
            activity: snippet.clone(),
        });
        if let Some(ref hub) = system_events {
            emit_task_progress_event(hub, task_id.to_string(), snippet);
        }
    }
}

async fn run_harness_in_process(
    args: Vec<String>,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    callback: bro_harness::emit::EventCallback,
    input_rx: bro_harness::agent_loop::SessionInputReceiver,
    mcp_config: Option<bro_harness::mcp::McpConfig>,
) -> anyhow::Result<()> {
    use clap::Parser;

    // Fully per-session, no process-global mutation, no lock (harness-daemon-
    // boundary.md §3): identity rides a task-local, the working directory is
    // passed as `--cwd` (the harness uses it for ToolCx.root instead of the
    // process cwd), shell children get their clean/augmented env from the shell
    // tool itself, and the daemon's own service env is scrubbed from those
    // children via with_spawn_scrub. Concurrent in-process sessions no longer
    // collide, so the previous serialize-everything lock is gone.
    let session_env: std::collections::BTreeMap<String, String> =
        env_overrides.unwrap_or_default().into_iter().collect();
    let scrub: Vec<String> = BLACKBOX_SERVICE_ENV_VARS
        .iter()
        .map(|k| k.to_string())
        .collect();

    let mut argv: Vec<String> = vec!["bro-harness".to_string()];
    if let Some(cwd) = cwd.as_deref() {
        argv.push("--cwd".to_string());
        argv.push(cwd.to_string());
    }
    argv.extend(args);
    let cli = bro_harness::cli::Cli::try_parse_from(argv)?;

    bro_tools::shell::with_spawn_scrub(
        scrub,
        bro_harness::transport::with_session_env(
            session_env,
            bro_harness::agent_loop::run_with_event_callback_and_input_mcp(
                cli, input_rx, callback, mcp_config,
            ),
        ),
    )
    .await
}

fn build_in_process_mcp_config(
    args: &mut Vec<String>,
    tool_placement: Option<BTreeMap<String, String>>,
) -> anyhow::Result<Option<bro_harness::mcp::McpConfig>> {
    let raw_mcp_config = take_cli_value_arg(args, "--mcp-config");
    let mut config = match raw_mcp_config {
        Some(raw) => bro_harness::mcp::McpConfig::from_json(&raw)?,
        None => bro_harness::mcp::McpConfig {
            servers: Vec::new(),
            tool_placement: Default::default(),
        },
    };
    add_transient_blackbox_mcp_server(&mut config);
    config.tool_placement = parse_dispatch_tool_placement(tool_placement)?;
    if config.servers.is_empty() && config.tool_placement.is_empty() {
        Ok(None)
    } else {
        Ok(Some(config))
    }
}

fn add_transient_blackbox_mcp_server(config: &mut bro_harness::mcp::McpConfig) {
    let Some(url) = std::env::var("BLACKBOX_MCP_URL")
        .ok()
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let name = crate::util::blackbox_mcp_name();
    if config.servers.iter().any(|server| server.name() == name) {
        return;
    }
    config
        .servers
        .push(bro_harness::mcp::McpServerConfig::Http {
            name,
            url,
            headers: BTreeMap::new(),
            exclude_tools: Vec::new(),
        });
}

fn take_cli_value_arg(args: &mut Vec<String>, flag: &str) -> Option<String> {
    let mut idx = 0;
    let mut found = None;
    while idx < args.len() {
        if args[idx] == flag {
            args.remove(idx);
            if idx < args.len() {
                found = Some(args.remove(idx));
            }
            continue;
        }
        idx += 1;
    }
    found
}

fn parse_dispatch_tool_placement(
    raw: Option<BTreeMap<String, String>>,
) -> anyhow::Result<bro_harness::mcp::ToolPlacementMap> {
    let mut out = bro_harness::mcp::ToolPlacementMap::new();
    let Some(raw) = raw else {
        return Ok(out);
    };
    for (name, placement) in raw {
        let parsed = match placement.as_str() {
            "in-box" => bro_harness::mcp::ToolPlacement::InBox,
            "out-box" => bro_harness::mcp::ToolPlacement::OutBox,
            "both" => bro_harness::mcp::ToolPlacement::Both,
            other => anyhow::bail!(
                "invalid tool_placement for {name}: {other}; expected in-box, out-box, or both"
            ),
        };
        out.insert(name, parsed);
    }
    Ok(out)
}

/// Spawn a task in **persistent bidirectional mode** (fleet-tui.md item 6): the
/// child's stdin is kept open and writable so the caller can drive successive
/// user-turns and `control_request`s over the stream-json control protocol. The
/// returned [`SpawnedTask`] carries that writable stdin. Reuses the full
/// one-shot spawn machinery (env hygiene, login-shell bin resolution,
/// stream-json reader, persistence registration, supervision); the only
/// difference is that stdin stays open.
///
/// Args should already include `--input-format stream-json` (and typically
/// `--replay-user-messages`); the initial `-p <prompt>` becomes the first user
/// turn, subsequent turns/controls are written to the returned stdin.
///
/// Orphaned by the §7 fleet-daemon-only cut (its only caller was the fleet
/// in-process launch). Kept until the daemon-side dispatch is consolidated;
/// remove together with the rest of the in-process spawn machinery.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub fn spawn_task_interactive(
    task_id: String,
    provider: Provider,
    args: Vec<String>,
    session_id: String,
    cwd: Option<String>,
    env_overrides: Option<HashMap<String, String>>,
    store_dir: std::path::PathBuf,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: tokio::sync::broadcast::Sender<tail::TailEvent>,
    bro_label: Option<String>,
    agent_label: Option<String>,
    system_events: Option<crate::system_events::SharedEventHub>,
) -> SpawnedTask {
    if let Err(err) = task_store.write().reserve_id(&task_id) {
        if let Some(existing) = task_store.read().get(&task_id) {
            return SpawnedTask {
                task: existing,
                stdin: None,
            };
        }
        return SpawnedTask {
            task: failed_duplicate_task(
                task_id,
                provider,
                session_id,
                cwd,
                bro_label,
                agent_label,
                err.to_string(),
            ),
            stdin: None,
        };
    }

    let params = SpawnTaskParams {
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        bro_label,
        agent_label,
        system_events,
        interactive: true,
    };

    spawn_task_reserved(task_id, params)
}

fn move_large_prompt_arg_to_stdin(provider: Provider, args: &mut Vec<String>) -> Option<String> {
    if !matches!(
        provider,
        Provider::Glm | Provider::Deepseek | Provider::Minimax
    ) {
        return None;
    }
    let mut idx = 0usize;
    let mut prompt_idx = None;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "-p" || arg == "--print" {
            let candidate_idx = idx + 1;
            if args
                .get(candidate_idx)
                .is_some_and(|candidate| candidate.len() >= PROMPT_STDIN_ARG_BYTES_THRESHOLD)
            {
                prompt_idx = Some(candidate_idx);
            }
            break;
        }
        if claude_family_option_takes_value(arg) && idx + 1 < args.len() {
            idx += 2;
        } else {
            idx += 1;
        }
    }
    let prompt_idx = prompt_idx?;
    Some(args.remove(prompt_idx))
}

fn claude_family_option_takes_value(arg: &str) -> bool {
    matches!(
        arg,
        "-m" | "--model"
            | "--permission-mode"
            | "--output-format"
            | "--input-format"
            | "--resume"
            | "--add-dir"
            | "--mcp-config"
            | "--append-system-prompt"
            | "--allowedTools"
            | "--disallowedTools"
    )
}

/// Open a per-session append file for tee-ing harness stdout/stderr, when
/// `BLACKBOX_HARNESS_TEE_DIR` is set (`bro fleet` sets it by default so fleet
/// spurious-stop turns are captured for postmortem). Returns None when disabled
/// or on any IO error — tee-ing is best-effort diagnostics and must never fail a
/// dispatch. `suffix` is e.g. "stdout.jsonl" / "stderr.log".
fn open_harness_tee(id: &str, suffix: &str) -> Option<std::fs::File> {
    let dir = std::env::var("BLACKBOX_HARNESS_TEE_DIR")
        .ok()
        .filter(|d| !d.is_empty())?;
    std::fs::create_dir_all(&dir).ok()?;
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::path::Path::new(&dir).join(format!("{id}.{suffix}")))
        .ok()
}

fn spawn_task_reserved(task_id: String, params: SpawnTaskParams) -> SpawnedTask {
    let SpawnTaskParams {
        provider,
        args,
        session_id,
        cwd,
        env_overrides,
        store_dir,
        task_store,
        tail_tx,
        bro_label,
        agent_label,
        system_events,
        interactive,
    } = params;
    let id = task_id;

    let path_env = providers::dispatch_path_env();

    // Resolve binary through a login shell so nvm/asdf/rbenv-installed CLIs
    // work even when the daemon was launched by launchctl/systemd with a
    // narrow PATH. Falls back to the bare name, which preserves the
    // existing error surface when the binary genuinely is not installed.
    let raw_bin = if let Ok(cfg) = blackbox::config::load() {
        provider.bin_with_config(&cfg.providers)
    } else {
        provider.bin()
    };
    let bin = providers::resolve_bin(&raw_bin).unwrap_or(raw_bin);
    let mut args = args;
    // Interactive sessions keep the prompt in `-p` (first user turn) and drive
    // later turns over stdin, so no large-prompt move; one-shot mode may still
    // spill an oversized prompt to stdin.
    let stdin_payload = if interactive {
        None
    } else {
        move_large_prompt_arg_to_stdin(provider, &mut args)
    };
    let mut cmd = Command::new(&bin);
    cmd.args(&args)
        .stdin(if interactive || stdin_payload.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
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
    for key in BLACKBOX_SERVICE_ENV_VARS {
        cmd.env_remove(key);
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
                    bro_label: bro_label.clone(),
                    agent_label: agent_label.clone(),
                    report: None,
                    recoverable: false,
                    transcript_location: None,
                    transcript_cursor: None,
                    supervision: SupervisionState::default(),
                }),
                notify: Arc::new(Notify::new()),
                child_id: Mutex::new(None),
            });
            let _ = task_store.write().insert_reserved(id, task.clone());
            task_store.read().persist(&store_dir);
            task.notify.notify_waiters();
            return SpawnedTask { task, stdin: None };
        }
    };

    let pid = child.id();
    // Interactive mode: keep stdin open and writable for the caller to drive the
    // persistent session. One-shot mode: write the spilled prompt then drop the
    // handle (closing stdin) as before.
    let interactive_stdin = if interactive {
        child.stdin.take()
    } else {
        if let Some(payload) = stdin_payload {
            if let Some(mut stdin) = child.stdin.take() {
                tokio::spawn(async move {
                    let _ = stdin.write_all(payload.as_bytes()).await;
                });
            }
        }
        None
    };
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
            bro_label,
            agent_label,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        }),
        notify: Arc::new(Notify::new()),
        child_id: Mutex::new(pid),
    });

    if let Err(err) = task_store.write().insert_reserved(id.clone(), task.clone()) {
        task_store.write().release_reservation(&id);
        let failed =
            failed_duplicate_task(id, provider, session_id, cwd, None, None, err.to_string());
        failed.notify.notify_waiters();
        return SpawnedTask {
            task: failed,
            stdin: None,
        };
    }

    // Emit tail event
    let _ = tail_tx.send(tail::TailEvent::TaskStarted {
        task_id: id.clone(),
        provider,
        bro_name: None,
    });
    // Emit task.started system event. Observation-only: failures logged, not propagated.
    if let Some(ref hub) = system_events {
        let task_id_ev = id.clone();
        let bro_ev = task.inner.lock().bro_label.clone();
        let provider_str = provider.to_string();
        let hub_clone = hub.clone();
        tokio::spawn(async move {
            let mut correlation = serde_json::Map::new();
            correlation.insert("task_id".into(), serde_json::json!(task_id_ev));
            let draft = crate::system_events::SystemEventDraft {
                kind: crate::system_events::types::SystemEventKind::TaskStarted,
                producer: "orchestration.dispatch".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation,
                causation_id: None,
                payload: serde_json::json!({
                    "task_id": task_id_ev,
                    "provider": provider_str,
                    "bro": bro_ev,
                }),
            };
            if let Err(e) = hub_clone.emit(draft).await {
                tracing::warn!("task.started system event emit failed: {e:#}");
            }
        });
    }

    // Spawn stdout reader — signals completion via oneshot so the process
    // waiter can ensure all output is consumed before marking the task done.
    let stdout = child.stdout.take().unwrap();
    let stderr_handle = child.stderr.take().unwrap();
    let task_ref = task.clone();
    let is_streaming = provider.is_streaming_json();
    let tail_tx_clone = tail_tx.clone();
    let task_id_clone = id.clone();
    let system_events_progress = system_events.clone();
    let disruption_store_dir = store_dir.clone();
    let disruption_task_id = id.clone();

    let (stdout_done_tx, stdout_done_rx) = tokio::sync::oneshot::channel::<()>();

    if is_streaming {
        // Line-by-line JSON parsing
        let tee_id_out = id.clone();
        tokio::spawn(async move {
            let reader = tokio::io::BufReader::new(stdout);
            let mut lines = reader.lines();
            let mut tee = open_harness_tee(&tee_id_out, "stdout.jsonl");
            let mut last_emitted_snippet: Option<String> = None;
            // Cooldown the lane the instant the provider returns a 429/overload,
            // so dispatch steers off it without waiting for the next probe tick.
            // Once per run is enough.
            let mut disruption_recorded = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if let Some(w) = tee.as_mut() {
                    use std::io::Write as _;
                    let _ = writeln!(w, "{line}");
                }
                if let Ok(evt) = serde_json::from_str::<Value>(&line) {
                    if !disruption_recorded {
                        if let Some(disruption) = provider.detect_disruption(&evt) {
                            disruption_recorded = true;
                            let account = allocator::lookup_lease_for_task(
                                &disruption_store_dir,
                                &disruption_task_id,
                            )
                            .and_then(|lease| lease.account);
                            account_probes::record_disruption_cooldown(
                                &disruption_store_dir,
                                provider,
                                account.as_deref(),
                                disruption,
                                now_ms(),
                            );
                        }
                    }
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
                        let emitted_session_id = sink.session_id.clone();
                        let mut accepted = true;
                        let mut session_id_observed = false;
                        if let Some(sid) = emitted_session_id {
                            if inner.session_id == "pending" {
                                inner.session_id = sid;
                                session_id_observed = true;
                            } else if inner.session_id != sid {
                                // Provider emitted a session_id that doesn't
                                // match the one we asked to resume. Mark failed
                                // and discard parsed output so the caller does
                                // not accidentally trust forked-session text.
                                reject_forked_session(&mut inner, &sid);
                                accepted = false;
                            }
                        }
                        if accepted {
                            apply_cwd_updates_from_event(&mut inner, &evt);
                            inner.supervision.observe_event(&evt, &sink, now_ms());
                            apply_sink_updates(&mut inner, sink);
                        }
                        accepted
                            .then(|| {
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
                            })
                            .flatten()
                            .map(|snippet| (snippet, session_id_observed))
                            .or_else(|| session_id_observed.then(|| (String::new(), true)))
                    };

                    if let Some((snippet, session_id_observed)) = snippet_to_emit {
                        if session_id_observed {
                            task_ref.notify.notify_waiters();
                        }
                        if !snippet.is_empty()
                            && last_emitted_snippet.as_deref() != Some(snippet.as_str())
                        {
                            let _ = tail_tx_clone.send(tail::TailEvent::TaskProgress {
                                task_id: task_id_clone.clone(),
                                activity: snippet.clone(),
                            });
                            // Emit task.progress system event. Observation-only: failures logged.
                            if let Some(ref hub) = system_events_progress {
                                emit_task_progress_event(
                                    hub,
                                    task_id_clone.clone(),
                                    snippet.clone(),
                                );
                            }
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
                let mut session_id_observed = false;
                if let Some(sid) = sink.session_id.clone() {
                    if inner.session_id == "pending" {
                        inner.session_id = sid;
                        session_id_observed = true;
                        inner.supervision.observe_bulk_sink(&sink, now_ms());
                        apply_sink_updates(&mut inner, sink);
                    } else if inner.session_id != sid {
                        reject_forked_session(&mut inner, &sid);
                    } else {
                        inner.supervision.observe_bulk_sink(&sink, now_ms());
                        apply_sink_updates(&mut inner, sink);
                    }
                } else {
                    inner.supervision.observe_bulk_sink(&sink, now_ms());
                    apply_sink_updates(&mut inner, sink);
                }
                if session_id_observed {
                    drop(inner);
                    task_ref_bulk.notify.notify_waiters();
                }
            }
            let _ = stdout_done_tx.send(());
        });
    }

    // Spawn stderr reader. Signals completion via oneshot so the waiter can
    // join it before snapshotting `inner.stderr` — without this, a fast fatal
    // exit (e.g. the harness bailing before any stdout) races the snapshot and
    // the task's `error` comes back empty, hiding the failure reason.
    let task_ref_err = task.clone();
    let tee_id_err = id.clone();
    let (stderr_done_tx, stderr_done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr_handle);
        let mut lines = reader.lines();
        let mut tee = open_harness_tee(&tee_id_err, "stderr.log");
        while let Ok(Some(line)) = lines.next_line().await {
            if let Some(w) = tee.as_mut() {
                use std::io::Write as _;
                let _ = writeln!(w, "{line}");
            }
            let mut inner = task_ref_err.inner.lock();
            inner.stderr.push_str(&line);
            inner.stderr.push('\n');
        }
        let _ = stderr_done_tx.send(());
    });

    // Spawn process waiter — waits for the process exit AND both readers
    // to finish before marking the task terminal. This ensures results are
    // fully parsed (stdout) and the failure reason is captured (stderr) before
    // waiters are notified.
    let task_ref_wait = task.clone();
    let task_id_wait = id.clone();
    let tail_tx_wait = tail_tx;
    let system_events_wait = system_events;
    tokio::spawn(async move {
        let status = child.wait().await;
        // Wait for stdout reader to finish before processing results —
        // ensures all events/results are parsed before we mark terminal.
        let _ = stdout_done_rx.await;
        // Join the stderr reader too, so `error_snippet` reflects the real
        // failure message rather than an empty (still-draining) buffer.
        let _ = stderr_done_rx.await;
        let code = status.ok().and_then(|s| s.code());

        let (terminal_status, elapsed, cost, error_snippet, source_session, task_kind) = {
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
            let terminal_status = inner.status;
            let cost = inner.cost_usd;
            let error_snippet: String = inner.stderr.chars().take(200).collect();
            let source_session = inner.session_id.clone();
            let task_kind = inner.bro_label.clone();
            (
                terminal_status,
                elapsed,
                cost,
                error_snippet,
                source_session,
                task_kind,
            )
        };
        match terminal_status {
            TaskStatus::Completed => {
                let _ = tail_tx_wait.send(tail::TailEvent::TaskCompleted {
                    task_id: task_id_wait.clone(),
                    elapsed: elapsed.clone(),
                    cost,
                    source_session,
                    task_kind,
                });
            }
            TaskStatus::Failed => {
                let _ = tail_tx_wait.send(tail::TailEvent::TaskFailed {
                    task_id: task_id_wait.clone(),
                    elapsed: elapsed.clone(),
                    error: error_snippet.clone(),
                });
            }
            _ => {}
        }
        // Emit terminal system event. Observation-only: failures logged, not propagated.
        // MutexGuard dropped above so the async emit is safe to await.
        if let Some(ref hub) = system_events_wait {
            let mut correlation = serde_json::Map::new();
            correlation.insert("task_id".into(), serde_json::json!(task_id_wait));
            let (kind, payload) = match terminal_status {
                TaskStatus::Completed => (
                    crate::system_events::types::SystemEventKind::TaskCompleted,
                    serde_json::json!({"task_id": task_id_wait, "elapsed": elapsed, "cost_usd": cost}),
                ),
                TaskStatus::Failed => (
                    crate::system_events::types::SystemEventKind::TaskFailed,
                    serde_json::json!({"task_id": task_id_wait, "elapsed": elapsed, "error": error_snippet}),
                ),
                TaskStatus::Cancelled => (
                    crate::system_events::types::SystemEventKind::TaskCancelled,
                    serde_json::json!({"task_id": task_id_wait, "elapsed": elapsed}),
                ),
                TaskStatus::Running => unreachable!("terminal state check above"),
            };
            let draft = crate::system_events::SystemEventDraft {
                kind,
                producer: "orchestration.dispatch".to_string(),
                project: None,
                principal: None,
                subject: None,
                correlation,
                causation_id: None,
                payload,
            };
            if let Err(e) = hub.emit(draft).await {
                tracing::warn!("task terminal system event emit failed: {e:#}");
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

    SpawnedTask {
        task,
        stdin: interactive_stdin,
    }
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

/// Wait for a provider-backed task to publish its real session id.
///
/// Wait for a late-discovered provider session id.
///
/// Modern harness-backed fresh dispatch pre-mints a concrete id before spawn.
/// This remains as a guard for legacy/provider-discovered paths: the
/// placeholder is internal state, and public authorial surfaces should either
/// return a concrete session id or diagnose the failed handshake.
pub async fn wait_for_task_session_id_with_timeout(
    task: &Task,
    timeout_secs: f64,
) -> Option<String> {
    async fn wait_for_session_id(task: &Task) -> Option<String> {
        loop {
            let notified = task.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let inner = task.inner.lock();
                if !inner.session_id.is_empty() && inner.session_id != "pending" {
                    return Some(inner.session_id.clone());
                }
                if inner.status.is_terminal() {
                    return None;
                }
            }
            notified.await;
        }
    }

    tokio::time::timeout(
        std::time::Duration::from_secs_f64(timeout_secs),
        wait_for_session_id(task),
    )
    .await
    .ok()
    .flatten()
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

fn apply_sink_updates(inner: &mut TaskInner, sink: EventSink) {
    // Merge — never CLEAR fields that were already captured during the
    // streaming run by overwriting with None from a partial update.
    if sink.last_assistant_message.is_some() {
        inner.last_assistant_message = sink.last_assistant_message;
    }
    if sink.usage.is_some() {
        inner.usage = sink.usage;
    }
    if sink.cost_usd.is_some() {
        inner.cost_usd = sink.cost_usd;
    }
    if sink.num_turns.is_some() {
        inner.num_turns = sink.num_turns;
    }
}

fn apply_cwd_updates_from_event(inner: &mut TaskInner, evt: &Value) {
    if let Some(payload) = successful_tool_result_payload(&inner.events, evt, "enter_worktree") {
        if let Some(cwd) = payload
            .get("cwd")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            inner.cwd = Some(cwd.to_string());
        }
    }
    if let Some(payload) = successful_tool_result_payload(&inner.events, evt, "exit_worktree") {
        if payload.get("removed_worktree").is_some()
            && let Some(base_repo) = payload
                .get("base_repo")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        {
            inner.cwd = Some(base_repo.to_string());
        }
    }
}

fn successful_tool_result_payload(events: &[Value], evt: &Value, tool_name: &str) -> Option<Value> {
    let mut tool_names = std::collections::HashMap::new();
    for event in events {
        let Some(blocks) = event
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(|c| c.as_array())
        else {
            continue;
        };
        for block in blocks {
            if block.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                && let (Some(id), Some(name)) = (
                    block.get("id").and_then(|id| id.as_str()),
                    block.get("name").and_then(|name| name.as_str()),
                )
            {
                tool_names.insert(id, name);
            }
        }
    }

    let blocks = evt
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_array())?;
    blocks.iter().find_map(|block| {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_result") {
            return None;
        }
        if block
            .get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            return None;
        }
        let id = block.get("tool_use_id").and_then(|id| id.as_str())?;
        if tool_names.get(id).copied() != Some(tool_name) {
            return None;
        }
        let content = tool_result_content_text(block.get("content"));
        let payload: Value = serde_json::from_str(&content).ok()?;
        payload
            .get("ok")
            .and_then(|ok| ok.as_bool())
            .unwrap_or(false)
            .then_some(payload)
    })
}

fn tool_result_content_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.get("text")
                    .and_then(|t| t.as_str())
                    .or_else(|| part.as_str())
            })
            .collect::<Vec<_>>()
            .join(""),
        Some(other) => other.to_string(),
        None => String::new(),
    }
}

fn reject_forked_session(inner: &mut TaskInner, emitted_session_id: &str) {
    if inner.status != TaskStatus::Failed {
        let requested = inner.session_id.clone();
        inner.status = TaskStatus::Failed;
        inner.stderr.push_str(&format!(
            "\nsession fork detected: requested resume of {requested}, provider emitted {emitted_session_id}"
        ));
    }
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
    populate_transcript_handle(task);
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
    obj["hasResult"] = Value::Bool(inner.last_assistant_message.is_some());
    if inner.status == TaskStatus::Completed || inner.status == TaskStatus::Failed {
        if let Some(ref u) = inner.usage {
            // `input_tokens` is fresh (cache-exclusive). Surface the cache
            // breakdown only when present so cache-free providers stay terse.
            let mut usage = serde_json::json!({
                "input_tokens": u.input_tokens,
                "output_tokens": u.output_tokens,
            });
            if u.cached_input_tokens > 0 || u.cache_creation_input_tokens > 0 {
                usage["cached_input_tokens"] = Value::from(u.cached_input_tokens);
                usage["cache_creation_input_tokens"] = Value::from(u.cache_creation_input_tokens);
                usage["total_input_tokens"] = Value::from(u.total_input_tokens());
            }
            obj["usage"] = usage;
        }
        if let Some(cost) = inner.cost_usd {
            obj["costUsd"] = Value::from(cost);
        }
        if let Some(turns) = inner.num_turns {
            obj["numTurns"] = Value::from(turns);
        }
        if inner.last_assistant_message.is_none() {
            obj["resultCapture"] = serde_json::json!({
                "status": "missing",
                "message": "task reached a terminal state without a captured assistant result",
                "eventCount": observed_event_count(&inner),
                "exitCode": inner.exit_code,
                "stderrPresent": !inner.stderr.trim().is_empty(),
                "transcriptLocated": inner.transcript_location.is_some(),
            });
        }
    }
    if let Some(ref label) = inner.bro_label {
        obj["broLabel"] = Value::String(label.clone());
    }
    if let Some(ref label) = inner.agent_label {
        obj["agentLabel"] = Value::String(label.clone());
    }
    if let Some(ref report) = inner.report {
        obj["report"] = report.to_json();
    }
    if let Some(ref location) = inner.transcript_location {
        obj["transcriptLocation"] = serde_json::to_value(location).unwrap_or(Value::Null);
    }
    if let Some(ref cursor) = inner.transcript_cursor {
        obj["transcriptCursor"] = serde_json::to_value(cursor).unwrap_or(Value::Null);
    }
    let supervision_now = inner.completed_at.unwrap_or_else(now_ms);
    obj["supervision"] = inner.supervision.snapshot_for_response(supervision_now);
    if inner.status == TaskStatus::Failed {
        if let Some(code) = inner.exit_code {
            obj["exitCode"] = Value::from(code);
        }
        if !inner.stderr.is_empty() {
            let truncated: String = inner.stderr.chars().take(2000).collect();
            obj["stderr"] = Value::String(truncated);
        }
        // Surface the recovery hint so calling agents can distinguish
        // "task killed by daemon restart, retry via bro_resume" from
        // "task genuinely failed, start fresh."
        if inner.recoverable && inner.provider != Provider::Workflow {
            obj["recoverable"] = Value::Bool(true);
            obj["recoveryHint"] = Value::String(format!(
                "retry with bro_resume(session_id=\"{}\", provider=\"{}\")",
                inner.session_id, inner.provider
            ));
        } else if inner.recoverable {
            obj["recoverable"] = Value::Bool(true);
            obj["recoveryHint"] = Value::String(
                "workflow task was interrupted by daemon restart; redispatch the workflow spec"
                    .into(),
            );
        }
    }
    obj
}

fn populate_transcript_handle(task: &Task) {
    let (provider, session_id, already_located) = {
        let inner = task.inner.lock();
        (
            inner.provider,
            inner.session_id.clone(),
            inner.transcript_location.is_some(),
        )
    };
    if already_located || session_id.is_empty() || session_id == "pending" {
        return;
    }
    let registry = TranscriptAdapterRegistry::from_runtime_config();
    let Ok(Some(location)) = registry.locate(provider, &session_id) else {
        return;
    };
    let mut inner = task.inner.lock();
    if inner.transcript_location.is_none() && inner.session_id == session_id {
        inner.transcript_location = Some(location);
    }
}

/// Project a task's core state into the shared `bro_protocol` wire DTO — the
/// status-plane half of the contract bottom (harness-daemon-boundary.md §7).
/// This is the typed snapshot the fleet client consumes from `/control/status`;
/// it is what gives `bro_protocol::TaskSnapshot`/`TaskStatus` a real producer.
/// (A free fn, not `From`, because the orphan rule forbids
/// `impl From<&TaskInner> for bro_protocol::TaskSnapshot` — both are foreign.)
pub fn protocol_task_snapshot(inner: &TaskInner) -> bro_protocol::TaskSnapshot {
    use bro_protocol::TaskStatus as Wire;
    let status = match inner.status {
        TaskStatus::Running => Wire::Running,
        TaskStatus::Completed => Wire::Completed,
        TaskStatus::Failed => Wire::Failed,
        TaskStatus::Cancelled => Wire::Cancelled,
    };
    let error = if matches!(inner.status, TaskStatus::Failed) && !inner.stderr.trim().is_empty() {
        Some(bro_core::BroError::new(
            "task_failed",
            inner.stderr.trim().to_string(),
        ))
    } else {
        None
    };
    bro_protocol::TaskSnapshot {
        task_id: bro_core::TaskId::new(inner.id.clone()),
        session_id: (!inner.session_id.is_empty())
            .then(|| bro_core::SessionId::new(inner.session_id.clone())),
        status,
        last_message: inner.last_assistant_message.clone(),
        error,
    }
}

pub fn task_status_json(task: &Task, tail: usize) -> Value {
    let mut obj = task_result_json(task);
    let inner = task.inner.lock();
    // Typed wire snapshot (additive field; existing ad-hoc fields stay for the
    // IRC bridge and other readers). The fleet poller deserializes this.
    obj["snapshot"] = serde_json::to_value(protocol_task_snapshot(&inner)).unwrap_or(Value::Null);
    let event_count = observed_event_count(&inner);
    obj["eventCount"] = Value::from(event_count);
    if tail > 0 && !inner.events.is_empty() {
        let mut recent: Vec<Value> = inner
            .events
            .iter()
            .rev()
            .filter_map(compact_status_event)
            .take(tail)
            .collect();
        recent.reverse();
        obj["recentEvents"] = Value::Array(recent);
    }
    // Surface the captured stderr tail when the task failed or emitted no
    // stream events — otherwise a pre-stream bail (e.g. the harness exiting
    // before any stdout) shows only exitCode:1 with no reason. Bounded so a
    // chatty stderr can't flood the response.
    if (inner.status == TaskStatus::Failed || event_count == 0) && !inner.stderr.trim().is_empty() {
        const MAX: usize = 2000;
        let s = inner.stderr.trim_end();
        let tail_str = if s.len() > MAX {
            let mut start = s.len() - MAX;
            while start < s.len() && !s.is_char_boundary(start) {
                start += 1;
            }
            format!("…{}", &s[start..])
        } else {
            s.to_string()
        };
        obj["stderrTail"] = Value::from(tail_str);
    }
    obj
}

fn compact_status_event(event: &Value) -> Option<Value> {
    if event.get("type").and_then(Value::as_str) == Some("stream_event") {
        return None;
    }
    let mut event = event.clone();
    strip_thinking_blocks(&mut event);
    bound_status_strings(&mut event);
    Some(event)
}

fn strip_thinking_blocks(value: &mut Value) {
    match value {
        Value::Array(items) => {
            items.retain(|item| item.get("type").and_then(Value::as_str) != Some("thinking"));
            for item in items {
                strip_thinking_blocks(item);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                strip_thinking_blocks(value);
            }
        }
        _ => {}
    }
}

fn bound_status_strings(value: &mut Value) {
    const MAX: usize = 2000;
    match value {
        Value::String(s) => {
            if s.len() > MAX {
                let mut end = MAX;
                while end > 0 && !s.is_char_boundary(end) {
                    end -= 1;
                }
                let omitted = s.len().saturating_sub(end);
                s.truncate(end);
                s.push_str(&format!("…[truncated {omitted} bytes]"));
            }
        }
        Value::Array(items) => {
            for item in items {
                bound_status_strings(item);
            }
        }
        Value::Object(map) => {
            for value in map.values_mut() {
                bound_status_strings(value);
            }
        }
        _ => {}
    }
}

pub fn timeout_snapshot_json(task: &Task) -> Value {
    let inner = task.inner.lock();
    let elapsed = format_elapsed(inner.started_at, None);
    let event_count = observed_event_count(&inner);
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
        "supervision": inner.supervision.snapshot_for_response(now_ms()),
    })
}

fn observed_event_count(inner: &TaskInner) -> usize {
    let supervision_count = usize::try_from(inner.supervision.event_count).unwrap_or(usize::MAX);
    inner.events.len().max(supervision_count)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    // Async env-mutating tests hold the std `test_env_lock` across `.await` on
    // purpose (env must stay set while the awaited code reads it); #[tokio::test]
    // is single-threaded so this can't deadlock the runtime.
    #![allow(clippy::await_holding_lock)]
    use super::*;

    #[test]
    fn apply_session_command_maps_protocol_variants_to_harness_input() {
        use bro_harness::agent_loop::{SessionInput, session_input_channel};
        use bro_protocol::SessionCommand;

        let task_id = "test-session-command-mapping";
        let (tx, mut rx) = session_input_channel();
        harness_controls().write().insert(task_id.to_string(), tx);

        // UserTurn -> User
        apply_session_command(task_id, SessionCommand::UserTurn { text: "hi".into() }).unwrap();
        match rx.try_recv().unwrap() {
            SessionInput::User(t) => assert_eq!(t, "hi"),
            other => panic!("expected User, got {other:?}"),
        }

        // Interrupt -> interrupt control
        apply_session_command(task_id, SessionCommand::Interrupt).unwrap();
        match rx.try_recv().unwrap() {
            SessionInput::Control { subtype, .. } => assert_eq!(subtype, "interrupt"),
            other => panic!("expected interrupt control, got {other:?}"),
        }

        // SetModel -> set_model control carrying the model in raw
        apply_session_command(task_id, SessionCommand::SetModel { model: "m2".into() }).unwrap();
        match rx.try_recv().unwrap() {
            SessionInput::Control { subtype, raw, .. } => {
                assert_eq!(subtype, "set_model");
                assert_eq!(raw["model"], "m2");
            }
            other => panic!("expected set_model control, got {other:?}"),
        }

        // Compact -> the /compact in-stream slash command (a genuine path, not a no-op)
        apply_session_command(task_id, SessionCommand::Compact).unwrap();
        match rx.try_recv().unwrap() {
            SessionInput::User(t) => assert_eq!(t, "/compact"),
            other => panic!("expected /compact user input, got {other:?}"),
        }

        harness_controls().write().remove(task_id);
    }

    #[test]
    fn apply_session_command_errors_without_live_channel() {
        let err =
            apply_session_command("no-such-live-task", bro_protocol::SessionCommand::Interrupt)
                .unwrap_err();
        assert!(err.contains("no live in-process harness control channel"));
    }

    #[test]
    fn in_process_mcp_config_strips_cli_arg_and_applies_dispatch_placement() {
        let mut env = crate::util::TestEnvGuard::new();
        env.set("BLACKBOX_MCP_URL", "http://127.0.0.1:7264/mcp");
        env.set("BLACKBOX_MCP_NAME", "selfbox");

        let mut args = vec![
            "--model".to_string(),
            "glm-test".to_string(),
            "--mcp-config".to_string(),
            serde_json::json!({
                "mcpServers": {
                    "external": {
                        "type": "stdio",
                        "command": "external-mcp",
                        "args": ["--once"]
                    }
                },
                "tool_placement": {
                    "mcp__external__ignored_json_source": "both"
                }
            })
            .to_string(),
            "--effort".to_string(),
            "low".to_string(),
        ];
        let config = build_in_process_mcp_config(
            &mut args,
            Some(BTreeMap::from([(
                "mcp__external__placed".to_string(),
                "in-box".to_string(),
            )])),
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            args,
            vec![
                "--model".to_string(),
                "glm-test".to_string(),
                "--effort".to_string(),
                "low".to_string()
            ]
        );
        assert_eq!(config.servers.len(), 2);
        assert!(config.servers.iter().any(|s| s.name() == "external"));
        assert!(config.servers.iter().any(|s| s.name() == "selfbox"));
        assert_eq!(
            config.tool_placement.get("mcp__external__placed"),
            Some(&bro_harness::mcp::ToolPlacement::InBox)
        );
        assert!(
            !config
                .tool_placement
                .contains_key("mcp__external__ignored_json_source")
        );
    }

    #[test]
    fn protocol_task_snapshot_maps_status_and_error() {
        // Completed task → wire Completed, last_message carried, no error.
        let t = test_task("t1", TaskStatus::Completed, Provider::Glm);
        t.inner.lock().last_assistant_message = Some("OK".to_string());
        let snap = protocol_task_snapshot(&t.inner.lock());
        assert_eq!(snap.task_id.as_str(), "t1");
        assert_eq!(
            snap.session_id.as_ref().map(|s| s.as_str()),
            Some("sess-t1")
        );
        assert_eq!(snap.status, bro_protocol::TaskStatus::Completed);
        assert_eq!(snap.last_message.as_deref(), Some("OK"));
        assert!(snap.error.is_none());

        // Failed task with stderr → error populated with a typed code.
        let f = test_task("t2", TaskStatus::Failed, Provider::Glm);
        f.inner.lock().stderr = "boom".to_string();
        let snap2 = protocol_task_snapshot(&f.inner.lock());
        assert_eq!(snap2.status, bro_protocol::TaskStatus::Failed);
        assert_eq!(
            snap2.error.as_ref().map(|e| e.code.as_str()),
            Some("task_failed")
        );

        // Cancelled maps through; running maps to wire Running.
        assert_eq!(
            protocol_task_snapshot(
                &test_task("t3", TaskStatus::Cancelled, Provider::Glm)
                    .inner
                    .lock()
            )
            .status,
            bro_protocol::TaskStatus::Cancelled
        );
        assert_eq!(
            protocol_task_snapshot(
                &test_task("t4", TaskStatus::Running, Provider::Glm)
                    .inner
                    .lock()
            )
            .status,
            bro_protocol::TaskStatus::Running
        );
    }

    #[test]
    fn test_task_status_is_terminal() {
        assert!(!TaskStatus::Running.is_terminal());
        assert!(TaskStatus::Completed.is_terminal());
        assert!(TaskStatus::Failed.is_terminal());
        assert!(TaskStatus::Cancelled.is_terminal());
    }

    fn task_with(status: TaskStatus, stderr: &str, events: Vec<Value>) -> Task {
        Task {
            inner: Mutex::new(TaskInner {
                id: "t".into(),
                provider: Provider::Glm,
                session_id: "s".into(),
                events,
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: stderr.into(),
                status,
                started_at: now_ms(),
                completed_at: Some(now_ms()),
                exit_code: Some(1),
                cwd: None,
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        }
    }

    fn persisted_event_count_after<F>(events: Vec<Value>, persist: F) -> usize
    where
        F: FnOnce(&TaskStore, &std::path::Path),
    {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let mut store = TaskStore::new();
        store
            .insert(
                "t".into(),
                Arc::new(task_with(TaskStatus::Failed, "", events)),
            )
            .unwrap();

        persist(&store, &root);

        TaskStore::load(&root, u64::MAX)
            .get("t")
            .unwrap()
            .inner
            .lock()
            .events
            .len()
    }

    #[test]
    fn task_store_default_persist_caps_events_but_full_persist_keeps_history() {
        let events: Vec<Value> = (0..60)
            .map(|idx| serde_json::json!({"type": "assistant", "idx": idx}))
            .collect();

        assert_eq!(
            persisted_event_count_after(events.clone(), |store, root| store.persist(root)),
            MAX_PERSISTED_EVENTS
        );
        assert_eq!(
            persisted_event_count_after(events, |store, root| store.persist_all_events(root)),
            60
        );
    }

    #[test]
    fn status_surfaces_stderr_tail_on_silent_failure() {
        // The bug this guards: a harness that bails before any stdout left
        // bro_status showing exit 1 / 0 events with no reason.
        let failed = task_with(
            TaskStatus::Failed,
            "harness error: no --model, no resumed session model, …",
            vec![],
        );
        let json = task_status_json(&failed, 0);
        assert_eq!(json["eventCount"], 0);
        assert!(
            json["stderrTail"].as_str().unwrap().contains("no --model"),
            "failed task must surface the stderr reason, got {json}"
        );
    }

    #[test]
    fn status_omits_stderr_tail_on_clean_success() {
        let ok = task_with(
            TaskStatus::Completed,
            "",
            vec![serde_json::json!({"type": "system"})],
        );
        let json = task_status_json(&ok, 0);
        assert!(json.get("stderrTail").is_none());
    }

    #[test]
    fn workload_retro_prompt_emits_scope_block() {
        let with_project = workload_retro_prompt("sess-123", Some("/repo/x"));
        assert!(with_project.starts_with("[scope] session:sess-123 · project:/repo/x\n\n"));
        assert!(with_project.contains("bbox_gap"));
        // The body is the canonical prompt, unmodified.
        assert!(with_project.ends_with(WORKLOAD_RETRO_PROMPT));

        // Project is optional — omit it cleanly rather than leaking an
        // empty segment.
        let no_project = workload_retro_prompt("sess-123", None);
        assert!(no_project.starts_with("[scope] session:sess-123\n\n"));
        assert!(!no_project.contains("project:"));
    }

    #[test]
    fn workload_retro_prompt_keeps_the_no_compulsion_balance() {
        // These phrases are load-bearing: they're what stop the probe from
        // manufacturing friction to satisfy a perceived quota. If a future
        // edit drops them, the balance is gone — fail loudly.
        for phrase in [
            "completely normal way for a run to end",
            "a quiet run is a good run",
            "don't manufacture friction",
            "file nothing",
        ] {
            assert!(
                WORKLOAD_RETRO_PROMPT.contains(phrase),
                "retro prompt lost the anti-compulsion phrase: {phrase:?}"
            );
        }
    }

    #[test]
    fn large_claude_family_prompt_moves_to_stdin() {
        let prompt = "x".repeat(PROMPT_STDIN_ARG_BYTES_THRESHOLD);
        let mut args = vec![
            "--resume".into(),
            "session-1".into(),
            "-p".into(),
            prompt.clone(),
            "--output-format".into(),
            "stream-json".into(),
        ];

        let stdin = move_large_prompt_arg_to_stdin(Provider::Glm, &mut args);

        assert_eq!(stdin.as_deref(), Some(prompt.as_str()));
        assert_eq!(
            args,
            vec![
                "--resume",
                "session-1",
                "-p",
                "--output-format",
                "stream-json"
            ]
        );

        let mut minimax_args = vec!["-p".into(), prompt.clone()];
        let minimax_stdin = move_large_prompt_arg_to_stdin(Provider::Minimax, &mut minimax_args);
        assert_eq!(minimax_stdin.as_deref(), Some(prompt.as_str()));
        assert_eq!(minimax_args, vec!["-p"]);
    }

    #[test]
    fn small_or_non_claude_prompt_stays_in_argv() {
        let mut small_args = vec!["-p".into(), "hello".into()];
        assert!(move_large_prompt_arg_to_stdin(Provider::Glm, &mut small_args).is_none());
        assert_eq!(small_args, vec!["-p", "hello"]);

        let prompt = "x".repeat(PROMPT_STDIN_ARG_BYTES_THRESHOLD);
        let mut codex_args = vec!["exec".into(), "--json".into(), prompt];
        assert!(move_large_prompt_arg_to_stdin(Provider::Brodex, &mut codex_args).is_none());
        assert_eq!(codex_args.len(), 3);
    }

    #[test]
    fn large_prompt_detection_skips_option_values_named_like_print_flag() {
        let prompt = "x".repeat(PROMPT_STDIN_ARG_BYTES_THRESHOLD);
        let mut args = vec![
            "--resume".into(),
            "-p".into(),
            "-p".into(),
            prompt.clone(),
            "--output-format".into(),
            "stream-json".into(),
        ];

        let stdin = move_large_prompt_arg_to_stdin(Provider::Glm, &mut args);

        assert_eq!(stdin.as_deref(), Some(prompt.as_str()));
        assert_eq!(
            args,
            vec!["--resume", "-p", "-p", "--output-format", "stream-json"]
        );
    }

    #[test]
    fn task_store_rejects_duplicate_task_ids_without_overwrite() {
        let mut store = TaskStore::new();
        let first = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-known".to_string(),
                provider: Provider::Brodex,
                session_id: "session-a".to_string(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        let second = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-known".to_string(),
                provider: Provider::Brodex,
                session_id: "session-b".to_string(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        store.insert("task-known".to_string(), first).unwrap();
        assert!(matches!(
            store.insert("task-known".to_string(), second),
            Err(BroSpawnError::DuplicateTaskId { .. })
        ));
        assert_eq!(
            store.get("task-known").unwrap().inner.lock().session_id,
            "session-a"
        );
    }

    #[test]
    fn ingest_is_error_result_marks_in_process_task_failed_and_captures_message() {
        // The in-process harness loop returns Ok regardless of turn outcome, so a
        // failed turn is surfaced only as a terminal `result {is_error:true}`
        // event. Ingesting it must fail the task and preserve the message
        // (gap-32113fd4) — the in-process analogue of a non-zero subprocess exit.
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-err".to_string(),
                provider: Provider::Minimax,
                session_id: "sess-err".to_string(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        let (tx, _rx) = tokio::sync::broadcast::channel(16);
        let evt = serde_json::json!({
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "session_id": "sess-err",
            "result": "anthropic messages 400 Bad Request: boom",
            "num_turns": 2,
        });
        ingest_harness_event(&task, Provider::Minimax, evt, &tx, "task-err", None);
        let inner = task.inner.lock();
        assert!(matches!(inner.status, TaskStatus::Failed));
        assert!(inner.stderr.contains("400 Bad Request: boom"));
    }

    #[test]
    fn task_store_reservation_blocks_duplicate_insert_until_used() {
        let mut store = TaskStore::new();
        store.reserve_id("task-reserved").unwrap();
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "task-reserved".to_string(),
                provider: Provider::Brodex,
                session_id: "session-a".to_string(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        assert!(matches!(
            store.insert("task-reserved".to_string(), task.clone()),
            Err(BroSpawnError::ReservedTaskId { .. })
        ));
        store
            .insert_reserved("task-reserved".to_string(), task)
            .unwrap();
        assert!(store.get("task-reserved").is_some());
    }

    #[tokio::test]
    async fn spawn_with_pre_minted_id_tracks_known_id() {
        let _env = crate::util::test_env_lock();
        let prior_bin = std::env::var("CODEX_BIN").ok();
        unsafe {
            std::env::set_var("CODEX_BIN", "/bin/true");
        }
        let tmp = tempfile::tempdir().unwrap();
        let task_store = Arc::new(RwLock::new(TaskStore::new()));
        let (tail_tx, _) = tokio::sync::broadcast::channel(8);
        let task = spawn_with_pre_minted_id(
            "task-known-id".to_string(),
            SpawnTaskParams {
                provider: Provider::Brodex,
                args: Vec::new(),
                session_id: "observed-session".to_string(),
                cwd: None,
                env_overrides: None,
                store_dir: tmp.path().to_path_buf(),
                task_store: task_store.clone(),
                tail_tx,
                bro_label: None,
                agent_label: None,
                system_events: None,
                interactive: false,
            },
        )
        .unwrap();

        assert_eq!(task.id(), "task-known-id");
        assert!(wait_for_task_with_timeout(&task, Some(2.0)).await);
        assert_eq!(
            task_store
                .read()
                .get("task-known-id")
                .unwrap()
                .inner
                .lock()
                .session_id,
            "observed-session"
        );

        match prior_bin {
            Some(value) => unsafe { std::env::set_var("CODEX_BIN", value) },
            None => unsafe { std::env::remove_var("CODEX_BIN") },
        }
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
            provider: Some(providers::Provider::Glm),
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
            providers::Provider::Glm,
            providers::Provider::VibeBh,
            providers::Provider::Brodex,
            providers::Provider::Deepseek,
            providers::Provider::VibeBh,
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
    fn ambient_allow_recursion_still_emits_scope_and_recall() {
        // Ambient prefix fires for every dispatch regardless of
        // `allow_recursion`. Recursion guarding is mechanical (tool
        // filter) not textual, and fan-out orchestrators still need
        // scope correlation + recall guidance + the packet nudge.
        let ctx = AmbientContext {
            session_id: Some("sess-orch".into()),
            allow_recursion: true,
            provider: Some(providers::Provider::Glm),
            ..Default::default()
        };
        let out = apply_ambient("coordinate stuff", &ctx);
        assert!(out.contains("[scope]"));
        assert!(out.contains("session: sess-orch"));
        assert!(out.contains("[recall before acting]"));
        assert!(out.contains("bbox_knowledge"));
        assert!(out.contains("[orchestrator]"));
        assert!(out.contains("bbox_compile"));
        assert!(out.contains("coordinate stuff"));
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
    fn ambient_emits_scoped_pin_block_when_present() {
        let ctx = AmbientContext {
            pin_block: Some(
                "- [bro:executor] Active arc: validate cuts against canonical doc".into(),
            ),
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[scoped pins]"));
        assert!(out.contains("Active arc"));
    }

    #[test]
    fn ambient_emits_recall_directive() {
        let ctx = AmbientContext::default();
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[recall before acting]"));
        assert!(out.contains("bbox_knowledge"));
        assert!(out.contains("durable knowledge"));
        assert!(out.contains("system runbooks"));
        assert!(out.contains("not the surface for scoped pins"));
        assert!(out.contains("short phrase"));
        assert!(!out.contains("FIRST tool call"));
    }

    #[test]
    fn ambient_task_shape_hint_fires_for_every_dispatch() {
        // Solo task: [task shape] should appear with bbox_compile +
        // bbox_packet_gap named. Addresses the S11 silent-bypass mode.
        let solo = AmbientContext {
            allow_recursion: false,
            ..Default::default()
        };
        let out_solo = apply_ambient("work", &solo);
        assert!(out_solo.contains("[task shape]"));
        assert!(out_solo.contains("bbox_compile"));
        assert!(out_solo.contains("bbox_packet_gap"));

        // Orchestrator task: both hints fire, composed.
        let orch = AmbientContext {
            allow_recursion: true,
            ..Default::default()
        };
        let out_orch = apply_ambient("coord", &orch);
        assert!(out_orch.contains("[task shape]"));
        assert!(out_orch.contains("[orchestrator]"));
        // Orchestrator hint follows task-shape hint in order.
        let shape_idx = out_orch.find("[task shape]").unwrap();
        let orch_idx = out_orch.find("[orchestrator]").unwrap();
        assert!(shape_idx < orch_idx);
    }

    #[test]
    fn ambient_orchestrator_hint_fires_under_allow_recursion() {
        // Fan-out orchestrators should see the packet-primitive nudge.
        // It's purely textual; the recursion guard is mechanical and
        // handled elsewhere via provider-specific tool filters.
        let ctx = AmbientContext {
            allow_recursion: true,
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[orchestrator]"));
        assert!(out.contains("bbox_compile"));
        assert!(out.contains("packet_id"));
    }

    #[test]
    fn ambient_orchestrator_hint_absent_without_recursion() {
        // Regular executors don't dispatch sub-agents, so the packet
        // nudge would be noise.
        let ctx = AmbientContext {
            allow_recursion: false,
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(!out.contains("[orchestrator]"));
    }

    #[test]
    fn coerce_workspace_false_omits_appendix() {
        let ctx = AmbientContext {
            coerce_workspace: false,
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(
            !out.contains("[workspace-tools mode]"),
            "appendix must not appear when coerce_workspace is false"
        );
    }

    #[test]
    fn coerce_workspace_true_injects_workspace_tools_appendix() {
        let ctx = AmbientContext {
            coerce_workspace: true,
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(
            out.contains("[workspace-tools mode]"),
            "appendix header must appear when coerce_workspace is true"
        );
        assert!(
            out.contains("work_smart_read"),
            "appendix must reference work_smart_read"
        );
        assert!(
            out.contains("work_bash"),
            "appendix must reference work_bash"
        );
        assert!(
            out.contains("work_git_status"),
            "appendix must reference work_git_status"
        );
        assert!(
            out.contains("work_git_diff"),
            "appendix must reference work_git_diff"
        );
        assert!(
            out.contains("work_git_log"),
            "appendix must reference work_git_log"
        );
        assert!(
            out.contains("bbox_note(kind=learned"),
            "appendix must reference bbox_note fallback"
        );
    }

    #[test]
    fn workspace_tools_appendix_placed_after_completion_contract() {
        let ctx = AmbientContext {
            coerce_workspace: true,
            completion_contract: Some("do the thing".into()),
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        let contract_idx = out.find("[completion contract]").unwrap();
        let ws_idx = out.find("[workspace-tools mode]").unwrap();
        assert!(
            contract_idx < ws_idx,
            "workspace-tools appendix must follow completion contract"
        );
    }

    #[test]
    fn coerce_workspace_true_composes_with_other_ambient_sections() {
        let ctx = AmbientContext {
            task_id: Some("task-123".into()),
            coerce_workspace: true,
            allow_recursion: true,
            completion_contract: Some("emit done".into()),
            provider: Some(providers::Provider::Glm),
            ..Default::default()
        };
        let out = apply_ambient("work", &ctx);
        assert!(out.contains("[scope]"));
        assert!(out.contains("[recall before acting]"));
        assert!(out.contains("[task shape]"));
        assert!(out.contains("[orchestrator]"));
        assert!(out.contains("[completion contract]"));
        assert!(out.contains("[workspace-tools mode]"));
        assert!(out.contains("work"));
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
            provider: Some(providers::Provider::Glm),
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
                provider: Provider::Glm,
                session_id: "s1".into(),
                events: vec![],
                last_assistant_message: Some("Done!".into()),
                usage: Some(Usage {
                    input_tokens: 100,
                    output_tokens: 50,
                    ..Default::default()
                }),
                cost_usd: Some(0.05),
                num_turns: Some(3),
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(5000),
                exit_code: Some(0),
                cwd: None,
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["taskId"], "t1");
        assert_eq!(json["result"], "Done!");
        assert_eq!(json["hasResult"], true);
        assert_eq!(json["costUsd"], 0.05);
        assert_eq!(json["usage"]["input_tokens"], 100);
        assert!(json["supervision"].is_object());
    }

    #[test]
    fn test_task_result_json_failed() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Brodex,
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["hasResult"], false);
        assert_eq!(json["resultCapture"]["status"], "missing");
        assert_eq!(json["resultCapture"]["stderrPresent"], true);
        assert_eq!(json["exitCode"], 1);
        assert!(
            json["stderr"]
                .as_str()
                .unwrap()
                .contains("something went wrong")
        );
    }

    #[test]
    fn task_result_json_completed_without_result_surfaces_capture_diagnostic() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-empty".into(),
                provider: Provider::Minimax,
                session_id: "s-empty".into(),
                events: vec![serde_json::json!({"type": "system", "subtype": "init"})],
                last_assistant_message: None,
                usage: None,
                cost_usd: None,
                num_turns: None,
                stderr: String::new(),
                status: TaskStatus::Completed,
                started_at: 1000,
                completed_at: Some(2000),
                exit_code: Some(0),
                cwd: None,
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let json = task_result_json(&task);
        assert_eq!(json["status"], "completed");
        assert_eq!(json["hasResult"], false);
        assert_eq!(json["resultCapture"]["status"], "missing");
        assert_eq!(
            json["resultCapture"]["message"],
            "task reached a terminal state without a captured assistant result"
        );
        assert_eq!(json["resultCapture"]["eventCount"], 1);
        assert_eq!(json["resultCapture"]["exitCode"], 0);
    }

    #[test]
    fn fork_rejection_marks_failed_without_overwriting_result_state() {
        let mut inner = TaskInner {
            id: "t3".into(),
            provider: Provider::Deepseek,
            session_id: "requested-session".into(),
            events: vec![],
            last_assistant_message: Some("trusted prior result".into()),
            usage: Some(Usage {
                input_tokens: 10,
                output_tokens: 5,
                ..Default::default()
            }),
            cost_usd: Some(0.01),
            num_turns: Some(1),
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: None,
            bro_label: None,
            agent_label: None,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        };

        reject_forked_session(&mut inner, "forked-session");

        assert_eq!(inner.status, TaskStatus::Failed);
        assert_eq!(
            inner.last_assistant_message.as_deref(),
            Some("trusted prior result")
        );
        assert_eq!(
            inner.stderr.trim(),
            "session fork detected: requested resume of requested-session, provider emitted forked-session"
        );
        assert_eq!(
            inner
                .usage
                .as_ref()
                .map(|u| (u.input_tokens, u.output_tokens)),
            Some((10, 5))
        );
        assert_eq!(inner.cost_usd, Some(0.01));
        assert_eq!(inner.num_turns, Some(1));
    }

    #[test]
    fn apply_sink_updates_replaces_task_observed_state() {
        let mut inner = TaskInner {
            id: "t4".into(),
            provider: Provider::Deepseek,
            session_id: "s1".into(),
            events: vec![],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: None,
            bro_label: None,
            agent_label: None,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        };

        let sink = EventSink {
            last_assistant_message: Some("fresh output".into()),
            usage: Some(Usage {
                input_tokens: 12,
                output_tokens: 8,
                ..Default::default()
            }),
            cost_usd: Some(0.02),
            num_turns: Some(2),
            session_id: Some("s1".into()),
        };

        apply_sink_updates(&mut inner, sink);

        assert_eq!(
            inner.last_assistant_message.as_deref(),
            Some("fresh output")
        );
        assert_eq!(
            inner
                .usage
                .as_ref()
                .map(|u| (u.input_tokens, u.output_tokens)),
            Some((12, 8))
        );
        assert_eq!(inner.cost_usd, Some(0.02));
        assert_eq!(inner.num_turns, Some(2));
    }

    #[test]
    fn successful_enter_worktree_updates_task_cwd() {
        let mut inner = TaskInner {
            id: "t5".into(),
            provider: Provider::Brodex,
            session_id: "s1".into(),
            events: vec![serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "enter1", "name": "enter_worktree" }
                ]}
            })],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: Some("/repo/base".into()),
            bro_label: None,
            agent_label: None,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        };
        let evt = serde_json::json!({
            "type": "user",
            "message": { "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "enter1",
                    "content": "{\"ok\":true,\"cwd\":\"/repo/.bro-fleet-worktrees/wt\",\"base_repo\":\"/repo/base\"}",
                    "is_error": false
                }
            ]}
        });
        inner.events.push(evt.clone());

        apply_cwd_updates_from_event(&mut inner, &evt);

        assert_eq!(inner.cwd.as_deref(), Some("/repo/.bro-fleet-worktrees/wt"));
    }

    #[test]
    fn successful_exit_worktree_with_removed_worktree_restores_base_cwd() {
        let mut inner = TaskInner {
            id: "t6".into(),
            provider: Provider::Brodex,
            session_id: "s1".into(),
            events: vec![serde_json::json!({
                "type": "assistant",
                "message": { "content": [
                    { "type": "tool_use", "id": "exit1", "name": "exit_worktree" }
                ]}
            })],
            last_assistant_message: None,
            usage: None,
            cost_usd: None,
            num_turns: None,
            stderr: String::new(),
            status: TaskStatus::Running,
            started_at: 1000,
            completed_at: None,
            exit_code: None,
            cwd: Some("/repo/.bro-fleet-worktrees/wt".into()),
            bro_label: None,
            agent_label: None,
            report: None,
            recoverable: false,
            transcript_location: None,
            transcript_cursor: None,
            supervision: SupervisionState::default(),
        };
        let evt = serde_json::json!({
            "type": "user",
            "message": { "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "exit1",
                    "content": "{\"ok\":true,\"disposition\":\"publish\",\"removed_worktree\":\"/repo/.bro-fleet-worktrees/wt\",\"base_repo\":\"/repo/base\"}",
                    "is_error": false
                }
            ]}
        });
        inner.events.push(evt.clone());

        apply_cwd_updates_from_event(&mut inner, &evt);

        assert_eq!(inner.cwd.as_deref(), Some("/repo/base"));
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
                provider: Provider::Glm,
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        // Should return immediately without blocking
        wait_for_task(&task).await;
    }

    #[tokio::test]
    async fn test_wait_for_task_session_id_observes_non_pending_id() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-session".into(),
                provider: Provider::Brodex,
                session_id: "pending".into(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });
        let task_clone = task.clone();
        tokio::spawn(async move {
            task_clone.inner.lock().session_id = "real-session".into();
            task_clone.notify.notify_waiters();
        });

        let session_id = wait_for_task_session_id_with_timeout(&task, 2.0).await;
        assert_eq!(session_id.as_deref(), Some("real-session"));
    }

    #[tokio::test]
    async fn test_wait_for_task_session_id_returns_none_on_terminal_pending() {
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t-session-terminal".into(),
                provider: Provider::Brodex,
                session_id: "pending".into(),
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        let session_id = wait_for_task_session_id_with_timeout(&task, 2.0).await;
        assert_eq!(session_id, None);
    }

    #[tokio::test]
    async fn test_wait_for_task_notify_race() {
        // Simulate the race: task completes between status check and await
        let task = Arc::new(Task {
            inner: Mutex::new(TaskInner {
                id: "t2".into(),
                provider: Provider::Glm,
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
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
                provider: Provider::Glm,
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
                bro_label: None,
                agent_label: None,
                report: None,
                recoverable: false,
                transcript_location: None,
                transcript_cursor: None,
                supervision: SupervisionState::default(),
            }),
            notify: Arc::new(Notify::new()),
            child_id: Mutex::new(None),
        });

        // Should timeout after 0.1s
        let completed = wait_for_task_with_timeout(&task, Some(0.1)).await;
        assert!(!completed, "should have timed out");
    }
}

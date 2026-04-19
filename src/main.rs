mod inbox;
mod index;
mod knowledge;
mod notes;
mod orchestration;
mod packets;
mod parser;
mod render;
mod system_memory;
mod threads;
mod tool_docs;
mod util;

use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::RwLock;

use axum::extract::{Query, State as AxumState};
use axum::response::sse::{Event, Sse};
use futures::stream::Stream;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, IntoContents, ServerCapabilities, ServerInfo};
use rmcp::schemars;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rmcp::{ServerHandler, tool, tool_handler, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use index::TranscriptIndex;
use knowledge::Knowledge;
use notes::Notes;
use orchestration::providers::{ExecOpts, Provider};
use orchestration::tail::TailEvent;
use orchestration::{self as orch, TaskStore};
use packets::Packets;
use threads::Threads;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

struct SharedState {
    idx: RwLock<TranscriptIndex>,
    kb: RwLock<Knowledge>,
    threads: RwLock<Threads>,
    notes: RwLock<Notes>,
    packets: RwLock<Packets>,
    task_store: Arc<RwLock<TaskStore>>,
    tail_tx: broadcast::Sender<TailEvent>,
    store_dir: PathBuf, // BRO_HOME (default: ~/.local/state/blackbox/bro)
}

// ---------------------------------------------------------------------------
// MCP Server Handler
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BlackboxServer {
    state: Arc<SharedState>,
    tool_router: ToolRouter<Self>,
}

impl BlackboxServer {
    fn new(state: Arc<SharedState>) -> Self {
        Self {
            state,
            tool_router: Self::bbox_tools() + Self::bro_tools(),
        }
    }

    fn ok_text(text: &str) -> CallToolResult {
        CallToolResult::success(text.to_string().into_contents())
    }

    fn ok_json(value: &Value) -> CallToolResult {
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        CallToolResult::success(text.into_contents())
    }

    fn err_text(msg: &str) -> CallToolResult {
        let mut r = CallToolResult::success(msg.to_string().into_contents());
        r.is_error = Some(true);
        r
    }

    /// Run a sync tool handler: time it, log at debug (ok) / warn (err),
    /// uniformly convert Result<String> into CallToolResult. Centralizes
    /// the match-ok-err boilerplate that used to repeat in every bbox_*
    /// handler and gives us per-call duration visibility in journald
    /// (filter: `journalctl --user -u blackbox | grep bbox_`).
    fn run<F>(tool: &'static str, op: F) -> CallToolResult
    where
        F: FnOnce() -> anyhow::Result<String>,
    {
        let start = std::time::Instant::now();
        match op() {
            Ok(text) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = text.len(), "ok");
                Self::ok_text(&text)
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Bbox tools (search, knowledge, threads)
// ---------------------------------------------------------------------------

use inbox::InboxParams;
use index::{
    CiteParams, ContextParams, MessagesParams, ReindexParams, SearchParams, SessionParams,
    SessionsListParams, TopicsParams,
};
use knowledge::{
    AbsorbParams, BootstrapParams, DecideParams, ForgetParams, KnowledgeListParams, LearnParams,
    RememberParams, RenderParams, ReviewParams,
};
use notes::{NoteListParams, NoteParams, NoteResolveParams};
use packets::{ApplyParams as PacketApplyParams, AuditParams, CompileParams};
use threads::{ThreadListParams, ThreadParams};

#[tool_router(router = bbox_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_search",
        description = "Full-text search across all indexed transcripts."
    )]
    fn bbox_search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        Self::run("bbox_search", || {
            let mut idx = self.state.idx.write();
            if idx.is_empty() {
                idx.build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            drop(idx);
            self.state.idx.read().search(&p)
        })
    }

    #[tool(
        name = "bbox_cite",
        description = "Trace a claim back to the turn that established it."
    )]
    fn bbox_cite(&self, Parameters(p): Parameters<CiteParams>) -> CallToolResult {
        Self::run("bbox_cite", || self.state.idx.read().cite(&p))
    }

    #[tool(
        name = "bbox_context",
        description = "Conversation context around a specific byte offset."
    )]
    fn bbox_context(&self, Parameters(p): Parameters<ContextParams>) -> CallToolResult {
        Self::run("bbox_context", || self.state.idx.read().context(&p))
    }

    #[tool(
        name = "bbox_session",
        description = "Summary metadata for a single session."
    )]
    fn bbox_session(&self, Parameters(p): Parameters<SessionParams>) -> CallToolResult {
        Self::run("bbox_session", || self.state.idx.read().session(&p))
    }

    #[tool(
        name = "bbox_messages",
        description = "Chronological messages from a session."
    )]
    fn bbox_messages(&self, Parameters(p): Parameters<MessagesParams>) -> CallToolResult {
        Self::run("bbox_messages", || self.state.idx.read().messages(&p))
    }

    #[tool(
        name = "bbox_reindex",
        description = "Build or incrementally update the search index."
    )]
    fn bbox_reindex(&self, Parameters(p): Parameters<ReindexParams>) -> CallToolResult {
        Self::run("bbox_reindex", || self.state.idx.write().reindex(&p))
    }

    #[tool(
        name = "bbox_topics",
        description = "Top terms in a session by frequency."
    )]
    fn bbox_topics(&self, Parameters(p): Parameters<TopicsParams>) -> CallToolResult {
        Self::run("bbox_topics", || self.state.idx.read().topics(&p))
    }

    #[tool(
        name = "bbox_sessions_list",
        description = "Browse sessions sorted by recency."
    )]
    fn bbox_sessions_list(&self, Parameters(p): Parameters<SessionsListParams>) -> CallToolResult {
        Self::run("bbox_sessions_list", || {
            self.state.idx.read().sessions_list(&p)
        })
    }

    #[tool(
        name = "bbox_stats",
        description = "Corpus statistics (doc count, index size, file counts)."
    )]
    fn bbox_stats(&self) -> CallToolResult {
        Self::run("bbox_stats", || self.state.idx.read().stats())
    }

    #[tool(
        name = "bbox_learn",
        description = "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files."
    )]
    fn bbox_learn(&self, Parameters(p): Parameters<LearnParams>) -> CallToolResult {
        Self::run("bbox_learn", || self.state.kb.write().learn(&p, false))
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    fn bbox_remember(&self, Parameters(p): Parameters<RememberParams>) -> CallToolResult {
        Self::run("bbox_remember", || {
            self.state.kb.write().remember(&p, false)
        })
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    fn bbox_decide(&self, Parameters(p): Parameters<DecideParams>) -> CallToolResult {
        Self::run("bbox_decide", || self.state.kb.write().decide(&p, false))
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query stored entries by free-text or filters. First tool call on any substantive task per the CORE RULE above. Also surfaces matching system memories (code-embedded runbooks) marked `[system]` when the query hits one."
    )]
    fn bbox_knowledge(&self, Parameters(p): Parameters<KnowledgeListParams>) -> CallToolResult {
        Self::run("bbox_knowledge", || {
            let runtime = self.state.kb.write().list(&p)?;
            // Also surface matching code-embedded memories. See
            // src/system_memory/ — these are static runbooks baked into the
            // binary via include_str!, queryable but never rendered.
            let memories = system_memory::search(p.query.as_deref());
            if memories.is_empty() {
                Ok(runtime)
            } else {
                let mut combined = runtime;
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                for m in memories {
                    combined.push_str(&system_memory::format_for_listing(m));
                    combined.push('\n');
                }
                Ok(combined)
            }
        })
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    fn bbox_forget(&self, Parameters(p): Parameters<ForgetParams>) -> CallToolResult {
        Self::run("bbox_forget", || self.state.kb.write().forget(&p))
    }

    #[tool(
        name = "bbox_render",
        description = "Render entries into CLAUDE.md / AGENTS.md / GEMINI.md."
    )]
    fn bbox_render(&self, Parameters(p): Parameters<RenderParams>) -> CallToolResult {
        Self::run("bbox_render", || self.state.kb.read().render(&p))
    }

    #[tool(
        name = "bbox_absorb",
        description = "Import external edits to rendered files back as unverified entries."
    )]
    fn bbox_absorb(&self, Parameters(p): Parameters<AbsorbParams>) -> CallToolResult {
        Self::run("bbox_absorb", || self.state.kb.write().absorb(&p))
    }

    #[tool(
        name = "bbox_lint",
        description = "Health check for contradictions, stale entries, duplicates."
    )]
    fn bbox_lint(&self) -> CallToolResult {
        Self::run("bbox_lint", || self.state.kb.read().lint())
    }

    #[tool(
        name = "bbox_review",
        description = "Approve or reject entries awaiting review."
    )]
    fn bbox_review(&self, Parameters(p): Parameters<ReviewParams>) -> CallToolResult {
        Self::run("bbox_review", || self.state.kb.write().review(&p))
    }

    #[tool(
        name = "bbox_bootstrap",
        description = "Onboard a new repo into the blackbox knowledge system."
    )]
    fn bbox_bootstrap(&self, Parameters(p): Parameters<BootstrapParams>) -> CallToolResult {
        Self::run("bbox_bootstrap", || self.state.kb.read().bootstrap(&p))
    }

    #[tool(
        name = "bbox_thread",
        description = "Open / continue / resolve / promote / rename / link a work thread."
    )]
    fn bbox_thread(&self, Parameters(p): Parameters<ThreadParams>) -> CallToolResult {
        Self::run("bbox_thread", || self.state.threads.write().thread(&p))
    }

    #[tool(
        name = "bbox_thread_list",
        description = "Scan threads by lifecycle status and idle age."
    )]
    fn bbox_thread_list(&self, Parameters(p): Parameters<ThreadListParams>) -> CallToolResult {
        Self::run("bbox_thread_list", || {
            self.state.threads.read().thread_list(&p)
        })
    }

    #[tool(
        name = "bbox_note",
        description = "Record a structured side-channel note while working."
    )]
    fn bbox_note(&self, Parameters(p): Parameters<NoteParams>) -> CallToolResult {
        Self::run("bbox_note", || self.state.notes.write().create(&p))
    }

    #[tool(
        name = "bbox_notes",
        description = "List / filter notes by kind, project, session, thread, resolution."
    )]
    fn bbox_notes(&self, Parameters(p): Parameters<NoteListParams>) -> CallToolResult {
        Self::run("bbox_notes", || self.state.notes.read().list(&p))
    }

    #[tool(
        name = "bbox_note_resolve",
        description = "Mark a note acknowledged or addressed."
    )]
    fn bbox_note_resolve(&self, Parameters(p): Parameters<NoteResolveParams>) -> CallToolResult {
        Self::run("bbox_note_resolve", || self.state.notes.write().resolve(&p))
    }

    #[tool(
        name = "bbox_inbox",
        description = "Aggregate attention layer across every store."
    )]
    fn bbox_inbox(&self, Parameters(p): Parameters<InboxParams>) -> CallToolResult {
        Self::run("bbox_inbox", || {
            let kb = self.state.kb.read();
            let threads = self.state.threads.read();
            let notes = self.state.notes.read();
            let task_store = self.state.task_store.read();
            inbox::compute_inbox(&kb, &threads, &notes, &task_store, &p)
        })
    }

    // ── Rule-packets (compressive compilation of observations) ────────

    #[tool(
        name = "bbox_compile",
        description = "Compile a set of rules into a rule-packet (a compiled axiomatic theory with a deterministic evaluator). Full workflow in `sm-rule-packets` — query via bbox_knowledge."
    )]
    fn bbox_compile(&self, Parameters(p): Parameters<CompileParams>) -> CallToolResult {
        Self::run("bbox_compile", || self.state.packets.read().compile(&p))
    }

    #[tool(
        name = "bbox_apply",
        description = "Evaluate a rule-packet against a single entity. Returns the first matching rule's consequent + rule_id."
    )]
    fn bbox_apply(&self, Parameters(p): Parameters<PacketApplyParams>) -> CallToolResult {
        Self::run("bbox_apply", || self.state.packets.read().apply_tool(&p))
    }

    #[tool(
        name = "bbox_audit",
        description = "Apply a packet to a `{entity, expected}[]` dataset; return fidelity report."
    )]
    fn bbox_audit(&self, Parameters(p): Parameters<AuditParams>) -> CallToolResult {
        Self::run("bbox_audit", || self.state.packets.read().audit_tool(&p))
    }
}

// ---------------------------------------------------------------------------
// Bro tools (orchestration)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ExecParams {
    /// Task instruction for the agent
    prompt: String,
    /// Named bro instance to target. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    bro: Option<String>,
    /// Raw provider for ad-hoc tasks
    #[serde(default)]
    provider: Option<String>,
    /// Working directory (absolute path)
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow patterns merged on top of global+project+brofile.
    /// Use to tighten or open the tool surface for this one invocation.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    /// Per-dispatch disallow patterns merged on top of global+project+brofile.
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct ResumeParams {
    /// Follow-up instruction
    prompt: String,
    /// Named bro instance to resume. Bare names must be unique across live
    /// teams; use `team::bro` to disambiguate.
    #[serde(default)]
    bro: Option<String>,
    /// Session ID from a prior task (requires provider)
    #[serde(default)]
    session_id: Option<String>,
    /// Provider (required with session_id)
    #[serde(default)]
    provider: Option<String>,
    /// Working directory
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays for this resume only.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WaitParams {
    /// Task ID from exec or resume
    task_id: String,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct WhenParams {
    /// Team name — waits on each member's most recent task
    #[serde(default)]
    team: Option<String>,
    /// Explicit list of task IDs
    #[serde(default)]
    task_ids: Option<Vec<String>>,
    /// Max seconds to wait (recommended: 120)
    #[serde(default)]
    timeout_seconds: Option<f64>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BroadcastParams {
    /// Team name
    team: String,
    /// Prompt sent to every member
    prompt: String,
    /// Working directory override
    #[serde(default)]
    project_dir: Option<String>,
    /// Skip anti-recursion guard (default: false)
    #[serde(default)]
    allow_recursion: Option<bool>,
    /// Per-dispatch allow/disallow overlays applied to every member.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct StatusParams {
    /// Task ID to check
    task_id: String,
    /// Number of recent events to include (default: 0)
    #[serde(default)]
    tail: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct DashboardParams {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    team: Option<String>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct CancelParams {
    /// Task ID to cancel
    task_id: String,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct PruneParams {
    /// Status to prune (failed, completed, cancelled). Defaults to
    /// "failed" — the only status that's almost always safe to drop
    /// without further filtering. Running tasks are never pruned.
    #[serde(default)]
    status: Option<String>,
    /// Optional provider filter (claude, codex, copilot, gemini, vibe).
    #[serde(default)]
    provider: Option<String>,
    /// Drop tasks that started more than this many hours ago.
    #[serde(default)]
    older_than_hours: Option<u64>,
    /// Dry-run: report what would be pruned without removing.
    /// Defaults to false — bro_prune is the explicit pruning verb.
    #[serde(default)]
    dry_run: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct BrofileParams {
    /// Operation: create, list, get, delete, set_account, list_accounts
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    account: Option<String>,
    #[serde(default)]
    lens: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    #[serde(default)]
    env: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    /// Persona-bound allow/disallow patterns embedded in the brofile.
    /// Apply at every dispatch using this brofile, between project
    /// mcp.json and per-dispatch ExecParams overrides.
    #[serde(default)]
    allow_tools: Option<Vec<String>>,
    #[serde(default)]
    disallow_tools: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamParams {
    /// Operation: save_template, list_templates, delete_template, create, list, dissolve, roster
    action: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    members: Option<Vec<TeamMemberSlot>>,
    #[serde(default)]
    template: Option<String>,
    #[serde(default)]
    project_dir: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    cancel_running: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
struct TeamMemberSlot {
    brofile: String,
    #[serde(default)]
    alias: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

// ---------------------------------------------------------------------------
// Progress notifications — MCP progressToken plumbing for blocking waits
// ---------------------------------------------------------------------------
//
// Per MCP spec, progress notifications are correlated to a pending request via
// the progressToken the caller put in `_meta`. The server MUST echo that exact
// token back; otherwise clients drop the notification as unknown. Servers MUST
// NOT send progress notifications unless the caller asked for them.

const PROGRESS_TICK_SECS: u64 = 15;

fn format_bro_line(task: &orch::Task, store_dir: &Path) -> (String, bool) {
    let inner = task.inner.lock();
    let terminal = inner.status.is_terminal();
    let bro_name = orchestration::team::find_bro_name_for_task(&inner.id, store_dir);
    let label = bro_name.unwrap_or_else(|| inner.id[..inner.id.len().min(8)].to_string());
    let elapsed = orch::format_elapsed(inner.started_at, inner.completed_at);
    let events = inner.events.len();
    let activity = if terminal {
        format!("{:?}", inner.status)
    } else {
        inner
            .last_assistant_message
            .as_deref()
            .map(|m| {
                let c = m.replace('\n', " ");
                if c.len() > 80 {
                    format!("{}…", &c[..80])
                } else {
                    c
                }
            })
            .unwrap_or_else(|| {
                if events == 0 {
                    "starting…".into()
                } else {
                    "working…".into()
                }
            })
    };
    (
        format!("[{label}] {elapsed} | {events} ev | {activity}"),
        terminal,
    )
}

fn format_progress_snapshot(tasks: &[Arc<orch::Task>], store_dir: &Path) -> (String, bool) {
    let mut all_terminal = true;
    let lines: Vec<String> = tasks
        .iter()
        .map(|t| {
            let (line, terminal) = format_bro_line(t, store_dir);
            if !terminal {
                all_terminal = false;
            }
            line
        })
        .collect();
    (lines.join("\n"), all_terminal)
}

/// Load the effective tool filter set for a dispatch (global + project
/// overlay + default recursion guard unless `allow_recursion`), then
/// translate to provider-specific CLI args. For Gemini, also writes a
/// per-dispatch policy file and returns the path so the caller can
/// clean it up after the child exits.
struct DispatchFilters {
    args: Vec<String>,
    /// Tempfile path for Gemini policy cleanup; None for other providers.
    policy_file: Option<PathBuf>,
}

/// Build a per-dispatch McpFilters overlay from a tool's allow/disallow
/// param vectors. Returns None when both are empty so callers can pass
/// None directly into resolve_dispatch_filters without an empty merge.
fn extra_filters_from_params(
    allow: Option<&[String]>,
    disallow: Option<&[String]>,
) -> Option<orchestration::mcp::McpFilters> {
    let allow = allow.unwrap_or(&[]);
    let disallow = disallow.unwrap_or(&[]);
    if allow.is_empty() && disallow.is_empty() {
        return None;
    }
    Some(orchestration::mcp::McpFilters {
        allow: allow.to_vec(),
        disallow: disallow.to_vec(),
    })
}

/// Combine brofile-embedded filters with per-dispatch params overlay.
/// Brofile applies first (persona scope), then per-dispatch (call scope).
/// Returns None when both are empty/absent.
fn combine_dispatch_filters(
    brofile_filters: Option<&orchestration::mcp::McpFilters>,
    params_filters: Option<&orchestration::mcp::McpFilters>,
) -> Option<orchestration::mcp::McpFilters> {
    match (brofile_filters, params_filters) {
        (None, None) => None,
        (Some(b), None) => Some(b.clone()),
        (None, Some(p)) => Some(p.clone()),
        (Some(b), Some(p)) => {
            let mut combined = b.clone();
            combined.merge_from(p);
            Some(combined)
        }
    }
}

fn resolve_dispatch_filters(
    provider: Provider,
    project_dir: Option<&str>,
    allow_recursion: bool,
    task_id: &str,
    extra: Option<&orchestration::mcp::McpFilters>,
) -> DispatchFilters {
    let global = orchestration::mcp::global_store_path()
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok())
        .unwrap_or_default();
    let project = project_dir
        .map(|pd| orchestration::mcp::project_store_path(Path::new(pd)))
        .and_then(|p| orchestration::mcp::McpStore::load(&p).ok());

    let mut eff = orchestration::mcp::resolve_effective(
        &global,
        project.as_ref(),
        /* include_default_guard */ !allow_recursion,
    );
    // Per-dispatch overlay merges last (after global, project, default
    // guard) so callers can tighten or open the surface for a single
    // invocation. Disallow patterns in `extra` add to the deny set;
    // allow patterns add to the allow set. Recursion guard still wins
    // because allow doesn't override disallow at provider level.
    if let Some(extra) = extra {
        eff.filters.merge_from(extra);
    }

    let mut args = provider.build_filter_args(&eff.filters);
    let mut policy_file = None;

    if provider == Provider::Gemini {
        match orchestration::mcp::write_gemini_policy_file(task_id, &eff.filters) {
            Ok(Some(path)) => {
                args.push("--policy".into());
                args.push(path.to_string_lossy().into_owned());
                policy_file = Some(path);
            }
            Ok(None) => { /* no filters → no file */ }
            Err(e) => tracing::warn!("gemini policy file write failed: {e:#}"),
        }
    }

    DispatchFilters { args, policy_file }
}

/// Delete a Gemini policy tempfile once the associated task reaches a
/// terminal state. Spawned as a detached tokio task from the dispatch
/// path. No-op if path is None.
fn cleanup_policy_file_when_done(task: std::sync::Arc<orch::Task>, path: Option<PathBuf>) {
    let Some(path) = path else { return };
    tokio::spawn(async move {
        loop {
            {
                let inner = task.inner.lock();
                if inner.status.is_terminal() {
                    break;
                }
            }
            tokio::select! {
                _ = task.notify.notified() => {}
                _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {}
            }
        }
        if let Err(e) = std::fs::remove_file(&path) {
            tracing::debug!("gemini policy cleanup {}: {e}", path.display());
        }
    });
}

fn spawn_progress_notifier(
    tasks: Vec<Arc<orch::Task>>,
    peer: rmcp::service::Peer<rmcp::RoleServer>,
    progress_token: rmcp::model::ProgressToken,
    store_dir: PathBuf,
) -> tokio::task::JoinHandle<()> {
    tracing::info!(target: "blackbox::progress", token = ?progress_token, tasks = tasks.len(), "notifier spawned");
    tokio::spawn(async move {
        let mut tick = 0u64;
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(PROGRESS_TICK_SECS)).await;
            tick += 1;

            let (msg, all_terminal) = format_progress_snapshot(&tasks, &store_dir);

            let send_result = peer
                .send_notification(rmcp::model::ServerNotification::ProgressNotification(
                    rmcp::model::Notification::new(rmcp::model::ProgressNotificationParam {
                        progress_token: progress_token.clone(),
                        progress: tick as f64,
                        total: None,
                        message: Some(msg.clone()),
                    }),
                ))
                .await;
            match send_result {
                Ok(()) => {
                    tracing::debug!(target: "blackbox::progress", tick, terminal = all_terminal, msg_len = msg.len(), "tick sent")
                }
                Err(e) => {
                    tracing::warn!(target: "blackbox::progress", tick, error = %e, "tick send failed")
                }
            }

            if all_terminal {
                break;
            }
        }
    })
}

#[tool_router(router = bro_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_exec",
        description = "Launch an agent task. Returns {taskId, sessionId} immediately."
    )]
    async fn bro_exec(&self, Parameters(p): Parameters<ExecParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let store_dir = self.state.store_dir.clone();

        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters) = match self
            .resolve_exec_target(
                p.bro.as_deref(),
                p.provider.as_deref(),
                p.project_dir.as_deref(),
            ) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&e),
        };

        // Pre-generate task_id so it lands in the ambient [scope] block
        // before subprocess launch — the primary correlation key for
        // bbox_note emissions regardless of when the provider itself
        // emits a session ID.
        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if provider == Provider::Claude {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
        };
        let final_prompt = orch::apply_brofile_lens(
            &orch::apply_ambient(&p.prompt, &ambient_ctx),
            lens.as_deref(),
        );
        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
        );
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );

        // Register Gemini policy-file cleanup once the task terminates.
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        // If targeting a named bro in a team, record the task
        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_resume",
        description = "Continue an existing session with a follow-up."
    )]
    async fn bro_resume(&self, Parameters(p): Parameters<ResumeParams>) -> CallToolResult {
        let store_dir = self.state.store_dir.clone();

        let (provider, session_id, _lens, exec_opts, env_overrides, cwd, brofile_filters) =
            match self.resolve_resume_target(
                p.bro.as_deref(),
                p.session_id.as_deref(),
                p.provider.as_deref(),
                p.project_dir.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => return Self::err_text(&e),
            };

        if !provider.supports_resume() {
            return Self::err_text(&format!("{provider} does not support resume"));
        }

        // Auto-resolve cwd from the session's own recorded origin so
        // agents can resurrect each other across repo boundaries without
        // the caller threading project_dir. Gemini gets a hard refuse on
        // miss because its CLI silently forks a fresh session when the
        // UUID isn't in the cwd's project hash folder (aliasing the
        // resumed session). Claude/Codex error loudly on miss — fall
        // through to the caller's cwd and let them surface the failure.
        let cwd = match provider.resolve_session_cwd(&session_id) {
            Some(p) => Some(p.to_string_lossy().into_owned()),
            None if provider == Provider::Gemini => {
                return Self::err_text(&format!(
                    "Gemini session {session_id} not found in ~/.gemini/tmp/*/chats. Refusing to resume because Gemini silently forks a new session when the UUID isn't in the cwd's project folder (aliasing the resumed session). Verify the session ID or re-dispatch.",
                ));
            }
            None => cwd,
        };

        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let task_id = uuid::Uuid::new_v4().to_string();

        // Re-apply ambient on resume: each resume is its own dispatch with a
        // fresh task_id, and the per-turn recall directive + completion
        // contract need to ride with every follow-up (memory-file
        // reinforcement decays at depth). The brofile lens was injected on
        // exec and lives in the transcript — not re-prepended here.
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
        };
        let wrapped_prompt = orch::apply_ambient(&p.prompt, &ambient_ctx);

        let mut args = provider.build_resume_args(&session_id, &wrapped_prompt, exec_opts.as_ref());
        // Filters (mechanical recursion guard + user-configured allow/
        // disallow) must ride with every dispatch — exec AND resume.
        // Without this, a resumed session re-acquires the orchestration
        // tool surface the recursion guard was meant to deny.
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
        );
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_wait",
        description = "Block until a single task completes."
    )]
    async fn bro_wait(
        &self,
        Parameters(p): Parameters<WaitParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };

        let caller_token = context.meta.get_progress_token();
        tracing::info!(target: "blackbox::progress", tool = "bro_wait", has_token = caller_token.is_some(), token = ?caller_token, "entry");
        let progress_handle = caller_token.map(|token| {
            spawn_progress_notifier(
                vec![task.clone()],
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        if completed {
            Self::ok_json(&orch::task_result_json(&task))
        } else {
            Self::ok_json(&orch::timeout_snapshot_json(&task))
        }
    }

    #[tool(
        name = "bro_when_all",
        description = "Block until ALL tasks / team members complete."
    )]
    async fn bro_when_all(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        let progress_handle = context.meta.get_progress_token().map(|token| {
            spawn_progress_notifier(
                tasks.clone(),
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        // Wait concurrently (like Promise.all), not sequentially
        let timeout = p.timeout_seconds;
        let store_dir = self.state.store_dir.clone();
        let futs: Vec<_> = tasks
            .iter()
            .map(|task| {
                let task = task.clone();
                let sd = store_dir.clone();
                async move {
                    let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
                    let bro_name = {
                        let inner = task.inner.lock();
                        orchestration::team::find_bro_name_for_task(&inner.id, &sd)
                    };
                    let mut r = if completed {
                        orch::task_result_json(&task)
                    } else {
                        orch::timeout_snapshot_json(&task)
                    };
                    if let Some(name) = bro_name {
                        r["bro"] = Value::String(name);
                    }
                    r
                }
            })
            .collect();

        let results: Vec<Value> = futures::future::join_all(futs).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        let all_done = results.iter().all(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "all_completed": all_done, "results": results }))
    }

    #[tool(
        name = "bro_when_any",
        description = "Block until the FIRST task completes."
    )]
    async fn bro_when_any(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        // Check if any already done
        let any_done = tasks.iter().any(|t| t.inner.lock().status.is_terminal());
        let progress_handle = if !any_done && !tasks.is_empty() {
            context.meta.get_progress_token().map(|token| {
                spawn_progress_notifier(
                    tasks.clone(),
                    context.peer.clone(),
                    token,
                    self.state.store_dir.clone(),
                )
            })
        } else {
            None
        };

        if !any_done && !tasks.is_empty() {
            // Race them
            let futs: Vec<_> = tasks
                .iter()
                .map(|t| {
                    let t = t.clone();
                    Box::pin(async move {
                        orch::wait_for_task(&t).await;
                    })
                })
                .collect();

            match p.timeout_seconds {
                Some(secs) => {
                    let dur = std::time::Duration::from_secs_f64(secs);
                    let _ = tokio::time::timeout(dur, futures::future::select_all(futs)).await;
                }
                None => {
                    futures::future::select_all(futs).await;
                }
            }
        }
        if let Some(h) = progress_handle {
            h.abort();
        }

        let mut results = Vec::new();
        for task in &tasks {
            let inner = task.inner.lock();
            let bro_name =
                orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
            drop(inner);

            let mut r = if task.inner.lock().status.is_terminal() {
                orch::task_result_json(task)
            } else {
                orch::timeout_snapshot_json(task)
            };
            if let Some(name) = bro_name {
                r["bro"] = Value::String(name);
            }
            results.push(r);
        }

        let any_completed = results.iter().any(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "any_completed": any_completed, "results": results }))
    }

    #[tool(
        name = "bro_broadcast",
        description = "Send the same prompt to every team member."
    )]
    async fn bro_broadcast(&self, Parameters(p): Parameters<BroadcastParams>) -> CallToolResult {
        let _team_lock = orchestration::team::lock_teams();
        let team = match orchestration::team::load_team(&p.team, &self.state.store_dir) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown team: {}", p.team)),
        };
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let cwd = p.project_dir.or(team.project_dir.clone());
        let store_dir = self.state.store_dir.clone();
        let mut launched = Vec::new();
        let mut updated_team = team.clone();
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());

        for (i, member) in team.members.iter().enumerate() {
            let brofile = match orchestration::brofile::resolve_brofile(
                &member.brofile,
                &store_dir,
                team.project_dir.as_deref(),
            ) {
                Some(bf) => bf,
                None => {
                    launched.push(json!({"bro": member.name, "error": format!("Brofile not found: {}", member.brofile)}));
                    continue;
                }
            };

            let mut env_overrides = None;
            if let Some(ref acct_name) = brofile.account {
                if let Some(acct) = orchestration::brofile::load_account(acct_name, &store_dir) {
                    env_overrides = acct.env;
                }
            }
            let exec_opts = if brofile.model.is_some() || brofile.effort.is_some() {
                Some(ExecOpts {
                    model: brofile.model.clone(),
                    effort: brofile.effort.clone(),
                })
            } else {
                None
            };
            // Per-member combined extra: brofile.filters + broadcast-level
            // params overlay. Recursion guard is added inside
            // resolve_dispatch_filters; both layers above merge on top.
            let extra = combine_dispatch_filters(brofile.filters.as_ref(), params_extra.as_ref());

            // Build first-turn prompt with ambient scope + brofile lens.
            // Only applies on fresh-session exec paths; resumes use the
            // raw prompt so ambient/lens aren't re-injected each turn.
            let build_exec_prompt = |task_id: &str, session_id: &str| -> String {
                let ctx = orch::AmbientContext {
                    task_id: Some(task_id.to_string()),
                    session_id: Some(session_id.to_string()),
                    project_dir: cwd.clone(),
                    bro_name: Some(member.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    completion_contract: if allow_recursion {
                        None
                    } else {
                        Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
                    },
                    allow_recursion,
                    provider: Some(brofile.provider),
                };
                orch::apply_brofile_lens(
                    &orch::apply_ambient(&p.prompt, &ctx),
                    brofile.lens.as_deref(),
                )
            };

            let task = if let Some(ref sid) = member.session_id {
                if sid != "pending" {
                    // Auto-resolve cwd from the session's origin so a
                    // broadcast can resurrect members even when the
                    // current team.project_dir differs from where each
                    // member's session was recorded. Gemini refuses on
                    // miss (silent-fork aliasing); claude/codex fall
                    // through and error loudly themselves.
                    let member_cwd = match brofile.provider.resolve_session_cwd(sid) {
                        Some(p) => Some(p.to_string_lossy().into_owned()),
                        None if brofile.provider == Provider::Gemini => {
                            launched.push(json!({
                                "bro": member.name,
                                "error": format!("Gemini session {sid} not found in ~/.gemini/tmp/*/chats — refusing to resume (silent-fork aliasing)"),
                            }));
                            continue;
                        }
                        None => cwd.clone(),
                    };
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let mut args =
                        brofile
                            .provider
                            .build_resume_args(sid, &p.prompt, exec_opts.as_ref());
                    let df = resolve_dispatch_filters(
                        brofile.provider,
                        member_cwd.as_deref(),
                        allow_recursion,
                        &task_id,
                        extra.as_ref(),
                    );
                    args.extend(df.args);
                    let t = orch::spawn_task(
                        task_id,
                        brofile.provider,
                        args,
                        sid.clone(),
                        member_cwd,
                        env_overrides,
                        store_dir.clone(),
                        self.state.task_store.clone(),
                        self.state.tail_tx.clone(),
                    );
                    cleanup_policy_file_when_done(t.clone(), df.policy_file);
                    t
                } else {
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let session_id = if brofile.provider == Provider::Claude {
                        uuid::Uuid::new_v4().to_string()
                    } else {
                        "pending".into()
                    };
                    let exec_prompt = build_exec_prompt(&task_id, &session_id);
                    let mut args = brofile.provider.build_exec_args(
                        &exec_prompt,
                        &session_id,
                        cwd.as_deref(),
                        exec_opts.as_ref(),
                    );
                    let df = resolve_dispatch_filters(
                        brofile.provider,
                        cwd.as_deref(),
                        allow_recursion,
                        &task_id,
                        extra.as_ref(),
                    );
                    args.extend(df.args);
                    let t = orch::spawn_task(
                        task_id,
                        brofile.provider,
                        args,
                        session_id,
                        cwd.clone(),
                        env_overrides,
                        store_dir.clone(),
                        self.state.task_store.clone(),
                        self.state.tail_tx.clone(),
                    );
                    cleanup_policy_file_when_done(t.clone(), df.policy_file);
                    updated_team.members[i].session_id = Some(t.inner.lock().session_id.clone());
                    t
                }
            } else {
                let task_id = uuid::Uuid::new_v4().to_string();
                let session_id = if brofile.provider == Provider::Claude {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    "pending".into()
                };
                let exec_prompt = build_exec_prompt(&task_id, &session_id);
                let mut args = brofile.provider.build_exec_args(
                    &exec_prompt,
                    &session_id,
                    cwd.as_deref(),
                    exec_opts.as_ref(),
                );
                let df = resolve_dispatch_filters(
                    brofile.provider,
                    cwd.as_deref(),
                    allow_recursion,
                    &task_id,
                    extra.as_ref(),
                );
                args.extend(df.args);
                let t = orch::spawn_task(
                    task_id,
                    brofile.provider,
                    args,
                    session_id,
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                );
                cleanup_policy_file_when_done(t.clone(), df.policy_file);
                updated_team.members[i].session_id = Some(t.inner.lock().session_id.clone());
                t
            };

            let tid = task.id();
            updated_team.members[i].task_history.push(tid.clone());
            let sid = task.inner.lock().session_id.clone();
            launched.push(json!({"bro": member.name, "taskId": tid, "sessionId": sid}));
        }

        orchestration::team::save_team(&updated_team, &store_dir);
        Self::ok_json(&json!({"team": p.team, "tasks": launched}))
    }

    #[tool(
        name = "bro_status",
        description = "Non-blocking progress check on a task."
    )]
    fn bro_status(&self, Parameters(p): Parameters<StatusParams>) -> CallToolResult {
        match self.state.task_store.read().get(&p.task_id) {
            Some(task) => Self::ok_json(&orch::task_status_json(&task, p.tail.unwrap_or(0))),
            None => Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        }
    }

    #[tool(name = "bro_dashboard", description = "List recent tasks / sessions.")]
    fn bro_dashboard(&self, Parameters(p): Parameters<DashboardParams>) -> CallToolResult {
        let store = self.state.task_store.read();
        let limit = p.limit.unwrap_or(20);

        let filter_provider = p
            .provider
            .as_deref()
            .and_then(|s| s.parse::<Provider>().ok());
        let filter_status: Option<orch::TaskStatus> = p
            .status
            .as_deref()
            .and_then(|s| serde_json::from_str(&format!("\"{s}\"")).ok());

        let team_task_ids: Option<std::collections::HashSet<String>> =
            p.team.as_ref().and_then(|name| {
                let team = orchestration::team::load_team(name, &self.state.store_dir)?;
                Some(
                    team.members
                        .iter()
                        .flat_map(|m| m.task_history.clone())
                        .collect(),
                )
            });

        let mut with_ts: Vec<(u64, Value)> = store
            .all_tasks()
            .iter()
            .filter(|t| {
                let inner = t.inner.lock();
                if let Some(fp) = filter_provider {
                    if inner.provider != fp {
                        return false;
                    }
                }
                if let Some(fs) = filter_status {
                    if inner.status != fs {
                        return false;
                    }
                }
                if let Some(ref ids) = team_task_ids {
                    if !ids.contains(&inner.id) {
                        return false;
                    }
                }
                true
            })
            .map(|t| {
                let inner = t.inner.lock();
                let bro_name =
                    orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
                let mut entry = json!({
                    "taskId": inner.id,
                    "provider": inner.provider,
                    "sessionId": inner.session_id,
                    "status": inner.status,
                    "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                    "hasResult": inner.last_assistant_message.is_some(),
                });
                if let Some(name) = bro_name {
                    entry["bro"] = Value::String(name);
                }
                (inner.started_at, entry)
            })
            .collect();
        with_ts.sort_by(|a, b| b.0.cmp(&a.0));
        let entries: Vec<Value> = with_ts.into_iter().take(limit).map(|(_, e)| e).collect();

        Self::ok_json(&json!({"count": entries.len(), "tasks": entries}))
    }

    #[tool(
        name = "bro_prune",
        description = "Drop terminal tasks from the store + persisted tasks.json."
    )]
    fn bro_prune(&self, Parameters(p): Parameters<PruneParams>) -> CallToolResult {
        let target_status = p.status.as_deref().unwrap_or("failed");
        let allowed = ["failed", "completed", "cancelled"];
        if !allowed.contains(&target_status) {
            return Self::err_text(&format!(
                "status must be one of {:?} (got {:?}); running tasks are never pruned",
                allowed, target_status,
            ));
        }
        let parsed_status: orch::TaskStatus =
            match serde_json::from_str(&format!("\"{target_status}\"")) {
                Ok(s) => s,
                Err(e) => return Self::err_text(&format!("status parse: {e}")),
            };
        let filter_provider = p
            .provider
            .as_deref()
            .and_then(|s| s.parse::<Provider>().ok());
        let cutoff_ms = p
            .older_than_hours
            .map(|h| orch::now_ms().saturating_sub(h.saturating_mul(3600 * 1000)));
        let dry_run = p.dry_run.unwrap_or(false);

        let dropped: Vec<String> = if dry_run {
            self.state
                .task_store
                .read()
                .all_tasks()
                .iter()
                .filter_map(|t| {
                    let inner = t.inner.lock();
                    if inner.status != parsed_status {
                        return None;
                    }
                    if let Some(fp) = filter_provider {
                        if inner.provider != fp {
                            return None;
                        }
                    }
                    if let Some(cutoff) = cutoff_ms {
                        if inner.started_at >= cutoff {
                            return None;
                        }
                    }
                    Some(inner.id.clone())
                })
                .collect()
        } else {
            let mut store = self.state.task_store.write();
            let dropped = store.retain_drop(|t| {
                let inner = t.inner.lock();
                // Keep running tasks always.
                if inner.status == orch::TaskStatus::Running {
                    return true;
                }
                // Keep tasks that don't match the filter.
                if inner.status != parsed_status {
                    return true;
                }
                if let Some(fp) = filter_provider {
                    if inner.provider != fp {
                        return true;
                    }
                }
                if let Some(cutoff) = cutoff_ms {
                    if inner.started_at >= cutoff {
                        return true;
                    }
                }
                false
            });
            store.persist(&self.state.store_dir);
            dropped
        };

        Self::ok_json(&json!({
            "dryRun": dry_run,
            "status": target_status,
            "pruned": dropped.len(),
            "taskIds": dropped,
        }))
    }

    #[tool(name = "bro_cancel", description = "Cancel a running task (SIGTERM).")]
    fn bro_cancel(&self, Parameters(p): Parameters<CancelParams>) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };
        match orch::cancel_task(&task, &self.state.task_store, &self.state.store_dir) {
            Ok(()) => {
                let inner = task.inner.lock();
                let _ = self.state.tail_tx.send(TailEvent::TaskCancelled {
                    task_id: inner.id.clone(),
                    elapsed: orch::format_elapsed(inner.started_at, inner.completed_at),
                });
                Self::ok_json(&json!({
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "cancelled",
                }))
            }
            Err(e) => Self::err_text(&e),
        }
    }

    #[tool(
        name = "bro_providers",
        description = "List configured providers, binaries, models."
    )]
    fn bro_providers(&self) -> CallToolResult {
        let mut info = serde_json::Map::new();
        for p in Provider::ALL {
            let bin = p.bin();
            let resolved = orch::providers::resolve_bin(&bin);
            let mut entry = json!({
                "bin": bin,
                "found": resolved.is_some(),
                "supportsResume": p.supports_resume(),
            });
            if let Some(ref path) = resolved {
                entry["path"] = json!(path);
            }
            if !p.models().is_empty() {
                entry["models"] = serde_json::to_value(p.models()).unwrap_or_default();
            }
            if !p.efforts().is_empty() {
                entry["efforts"] = serde_json::to_value(p.efforts()).unwrap_or_default();
            }
            info.insert(p.as_str().to_string(), entry);
        }
        Self::ok_json(&Value::Object(info))
    }

    #[tool(
        name = "bro_brofile",
        description = "Manage brofile templates + accounts (provider+account+lens)."
    )]
    fn bro_brofile(&self, Parameters(p): Parameters<BrofileParams>) -> CallToolResult {
        use orchestration::brofile;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "create" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let provider = match p
                    .provider
                    .as_deref()
                    .and_then(|s| s.parse::<Provider>().ok())
                {
                    Some(p) => p,
                    None => return Self::err_text("valid provider is required"),
                };
                let filters = extra_filters_from_params(
                    p.allow_tools.as_deref(),
                    p.disallow_tools.as_deref(),
                );
                let bf = brofile::Brofile {
                    name: name.clone(),
                    provider,
                    account: p.account.clone(),
                    lens: p.lens.clone(),
                    model: p.model.clone(),
                    effort: p.effort.clone(),
                    filters,
                };
                brofile::save_brofile(&bf, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"created": name, "scope": scope, "brofile": bf}))
            }
            "list" => {
                let list = brofile::list_brofiles(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "get" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                match brofile::resolve_brofile(name, store_dir, p.project_dir.as_deref()) {
                    Some(bf) => Self::ok_json(&serde_json::to_value(&bf).unwrap_or_default()),
                    None => Self::err_text(&format!("Brofile not found: {name}")),
                }
            }
            "delete" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if brofile::delete_brofile(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Brofile not found: {name}"))
                }
            }
            "set_account" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let mut config = brofile::load_config(store_dir);
                config
                    .accounts
                    .insert(name.clone(), brofile::Account { env: p.env.clone() });
                brofile::save_config(&config, store_dir);
                Self::ok_json(&json!({"account": name, "env": p.env}))
            }
            "list_accounts" => {
                let config = brofile::load_config(store_dir);
                Self::ok_json(&serde_json::to_value(&config.accounts).unwrap_or_default())
            }
            _ => Self::err_text(&format!("Unknown brofile action: {}", p.action)),
        }
    }

    #[tool(
        name = "bro_mcp",
        description = "Manage MCP servers + tool filters for dispatched bros."
    )]
    fn bro_mcp(
        &self,
        Parameters(p): Parameters<orchestration::mcp::McpToolParams>,
    ) -> CallToolResult {
        Self::run("bro_mcp", || orchestration::mcp::handle(&p))
    }

    #[tool(
        name = "bro_team",
        description = "Manage teamplates and instantiated teams."
    )]
    fn bro_team(&self, Parameters(p): Parameters<TeamParams>) -> CallToolResult {
        use orchestration::team;
        let store_dir = &self.state.store_dir;
        let scope = p.scope.as_deref().unwrap_or("global");

        match p.action.as_str() {
            "save_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                let members = match &p.members {
                    Some(m) if !m.is_empty() => m,
                    _ => return Self::err_text("members is required"),
                };
                // Validate brofile names
                for m in members {
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        p.project_dir.as_deref(),
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let tp = team::Teamplate {
                    name: name.clone(),
                    members: members
                        .iter()
                        .map(|m| team::TeamplateMember {
                            brofile: m.brofile.clone(),
                            alias: m.alias.clone(),
                            count: m.count.unwrap_or(1),
                        })
                        .collect(),
                };
                team::save_teamplate(&tp, scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&json!({"saved": name, "scope": scope}))
            }
            "list_templates" => {
                let list = team::list_teamplates(scope, store_dir, p.project_dir.as_deref());
                Self::ok_json(&serde_json::to_value(&list).unwrap_or_default())
            }
            "delete_template" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                if scope == "project" && p.project_dir.is_none() {
                    return Self::err_text("project_dir required for project scope");
                }
                if team::delete_teamplate(name, scope, store_dir, p.project_dir.as_deref()) {
                    Self::ok_json(&json!({"deleted": name}))
                } else {
                    Self::err_text(&format!("Teamplate not found: {name}"))
                }
            }
            "create" => {
                let template = match &p.template {
                    Some(t) => t,
                    None => return Self::err_text("template is required"),
                };
                let tp =
                    match team::resolve_teamplate(template, store_dir, p.project_dir.as_deref()) {
                        Some(tp) => tp,
                        None => return Self::err_text(&format!("Teamplate not found: {template}")),
                    };
                // Validate all brofiles exist before instantiating
                for m in &tp.members {
                    if orchestration::brofile::resolve_brofile(
                        &m.brofile,
                        store_dir,
                        p.project_dir.as_deref(),
                    )
                    .is_none()
                    {
                        return Self::err_text(&format!("Brofile not found: {}", m.brofile));
                    }
                }
                let team_name = p
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("{template}-{}", orch::now_ms()));
                let t =
                    team::instantiate_team(&tp, &team_name, p.project_dir.as_deref(), store_dir);
                Self::ok_json(&json!({
                    "created": t.name,
                    "teamplate": tp.name,
                    "members": t.members.iter().map(|m| json!({"name": m.name, "brofile": m.brofile})).collect::<Vec<_>>(),
                }))
            }
            "list" => {
                let teams = team::load_all_teams(store_dir);
                let list: Vec<Value> = teams
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.name,
                            "teamplate": t.teamplate,
                            "memberCount": t.members.len(),
                            "createdAt": t.created_at,
                            "projectDir": t.project_dir,
                        })
                    })
                    .collect();
                Self::ok_json(&json!(list))
            }
            "dissolve" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                if p.cancel_running.unwrap_or(false) {
                    let task_store = self.state.task_store.read();
                    for member in &loaded_team.members {
                        for tid in &member.task_history {
                            if let Some(task) = task_store.get(tid) {
                                let _ = orch::cancel_task(
                                    &task,
                                    &self.state.task_store,
                                    &self.state.store_dir,
                                );
                            }
                        }
                    }
                }
                team::remove_team(name, store_dir);
                Self::ok_json(&json!({"dissolved": name}))
            }
            "roster" => {
                let name = match &p.name {
                    Some(n) => n,
                    None => return Self::err_text("name is required"),
                };
                let loaded_team = match team::load_team(name, store_dir) {
                    Some(t) => t,
                    None => return Self::err_text(&format!("Unknown team: {name}")),
                };
                let task_store = self.state.task_store.read();
                let roster: Vec<Value> = loaded_team
                    .members
                    .iter()
                    .map(|m| {
                        let latest_tid = m.task_history.last();
                        let latest = latest_tid.and_then(|id| task_store.get(id)).map(|t| {
                        let inner = t.inner.lock();
                        json!({
                            "taskId": inner.id,
                            "status": inner.status,
                            "elapsed": orch::format_elapsed(inner.started_at, inner.completed_at),
                        })
                    });
                        json!({
                            "name": m.name,
                            "brofile": m.brofile,
                            "sessionId": m.session_id,
                            "taskCount": m.task_history.len(),
                            "latestTask": latest,
                        })
                    })
                    .collect();
                Self::ok_json(
                    &json!({"team": name, "teamplate": loaded_team.teamplate, "members": roster}),
                )
            }
            _ => Self::err_text(&format!("Unknown team action: {}", p.action)),
        }
    }
}

// ---------------------------------------------------------------------------
// Helper methods on BlackboxServer
// ---------------------------------------------------------------------------

impl BlackboxServer {
    #[allow(clippy::type_complexity)]
    fn resolve_exec_target(
        &self,
        bro_name: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            match orchestration::team::resolve_bro_selector(name, &teams)? {
                Some(bro_match) => {
                    let member = &bro_match.team.members[bro_match.member_idx];
                    let bf = orchestration::brofile::resolve_brofile(
                        &member.brofile,
                        store_dir,
                        bro_match.team.project_dir.as_deref(),
                    )
                    .ok_or(format!("Brofile not found: {}", member.brofile))?;
                    let env = bf
                        .account
                        .as_ref()
                        .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                        .and_then(|a| a.env);
                    let opts = if bf.model.is_some() || bf.effort.is_some() {
                        Some(ExecOpts {
                            model: bf.model.clone(),
                            effort: bf.effort.clone(),
                        })
                    } else {
                        None
                    };
                    let cwd = project_dir
                        .map(String::from)
                        .or(bro_match.team.project_dir.clone());
                    return Ok((bf.provider, bf.lens, opts, env, cwd, bf.filters));
                }
                None => {
                    // Standalone brofile fallback
                }
            }
            let bf = orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                .ok_or(format!("Unknown bro or brofile: {name}"))?;
            let env = bf
                .account
                .as_ref()
                .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                .and_then(|a| a.env);
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            return Ok((
                bf.provider,
                bf.lens,
                opts,
                env,
                project_dir.map(String::from),
                bf.filters,
            ));
        }

        if let Some(p) = raw_provider {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            return Ok((
                provider,
                None,
                None,
                None,
                project_dir.map(String::from),
                None,
            ));
        }

        Err("Provide either bro or provider".into())
    }

    #[allow(clippy::type_complexity)]
    fn resolve_resume_target(
        &self,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            String,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            let bro_match = orchestration::team::resolve_bro_selector(name, &teams)?
                .ok_or_else(|| {
                    if orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                        .is_some()
                    {
                        format!(
                            "Brofile \"{name}\" is not in a team — use exec first or provide session_id + provider"
                        )
                    } else {
                        format!("Unknown bro: {name}")
                    }
                })?;
            let member = &bro_match.team.members[bro_match.member_idx];
            let sid = member
                .session_id
                .as_deref()
                .filter(|s| *s != "pending")
                .ok_or(format!(
                    "Bro \"{name}\" has no active session — use exec first"
                ))?;
            let bf = orchestration::brofile::resolve_brofile(
                &member.brofile,
                store_dir,
                bro_match.team.project_dir.as_deref(),
            )
            .ok_or(format!("Brofile not found: {}", member.brofile))?;
            let env = bf
                .account
                .as_ref()
                .and_then(|a| orchestration::brofile::load_account(a, store_dir))
                .and_then(|a| a.env);
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            let cwd = project_dir
                .map(String::from)
                .or(bro_match.team.project_dir.clone());
            return Ok((
                bf.provider,
                sid.to_string(),
                bf.lens,
                opts,
                env,
                cwd,
                bf.filters,
            ));
        }

        if let (Some(sid), Some(p)) = (session_id, raw_provider) {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            return Ok((
                provider,
                sid.to_string(),
                None,
                None,
                None,
                project_dir.map(String::from),
                None,
            ));
        }

        Err("Provide either bro or session_id + provider".into())
    }

    fn resolve_when_targets(
        &self,
        team_name: Option<&str>,
        task_ids: Option<&[String]>,
    ) -> Result<Vec<String>, String> {
        if let Some(name) = team_name {
            let team = orchestration::team::load_team(name, &self.state.store_dir)
                .ok_or(format!("Unknown team: {name}"))?;
            let ids: Vec<String> = team
                .members
                .iter()
                .filter_map(|m| m.task_history.last().cloned())
                .collect();
            if ids.is_empty() {
                return Err(format!("No tasks found for team {name}"));
            }
            return Ok(ids);
        }
        if let Some(ids) = task_ids {
            if ids.is_empty() {
                return Err("Empty task_ids array".into());
            }
            return Ok(ids.to_vec());
        }
        Err("Provide either team or task_ids".into())
    }

    fn record_task_to_bro(&self, bro_name: &str, task: &Arc<orch::Task>) {
        let _lock = orchestration::team::lock_teams();
        let tid = task.id();
        let teams = orchestration::team::load_all_teams(&self.state.store_dir);
        let Ok(bro_match_opt) = orchestration::team::resolve_bro_selector(bro_name, &teams) else {
            return;
        };
        let Some(bro_match) = bro_match_opt else {
            return;
        };
        let target_team = bro_match.team.name.clone();
        let target_member_idx = bro_match.member_idx;
        let task_sid = task.inner.lock().session_id.clone();

        for mut team in teams {
            if team.name != target_team {
                continue;
            }
            let member = &mut team.members[target_member_idx];
            member.task_history.push(tid.clone());
            // Track the latest launch. Skip "pending" — late propagation
            // will fill it in once the provider discovers its session.
            if task_sid != "pending" {
                member.session_id = Some(task_sid.clone());
            }
            orchestration::team::save_team(&team, &self.state.store_dir);
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Blackbox: unified transcript search, knowledge management, and multi-provider agent orchestration")
    }
}

// ---------------------------------------------------------------------------
// Bro roster endpoint — resolves selectors to concrete per-bro lane info
// (provider, session_id, transcript file path). Consumed by `bro tail`
// to know WHICH JSONL files to open and follow.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RosterQuery {
    /// Comma-separated bro names (union of matches across all teams)
    #[serde(default)]
    bros: Option<String>,
    /// Comma-separated team names (each contributes all members). Accepts
    /// legacy `team=` singular form as an alias.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated session IDs — synthetic adhoc lanes bypassing team membership.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names (claude/codex/gemini/copilot/vibe) — final filter.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

#[derive(Debug, Serialize)]
struct BroRosterEntry {
    bro: String,
    bro_selector: String,
    team: String,
    provider: String,
    session_id: Option<String>,
    jsonl_path: Option<String>,
    brofile: String,
    model: Option<String>,
}

fn split_csv(s: &Option<String>) -> Vec<String> {
    s.as_deref()
        .unwrap_or("")
        .split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

fn infer_provider_from_path(path: &Path) -> Option<Provider> {
    let s = path.to_string_lossy();
    if s.contains("/.codex/sessions/") {
        Some(Provider::Codex)
    } else if s.contains("/.gemini/tmp/") {
        Some(Provider::Gemini)
    } else if s.contains("/.copilot/session-state/") {
        Some(Provider::Copilot)
    } else if s.contains("/.vibe/logs/session/") {
        Some(Provider::Vibe)
    } else if s.contains("/projects/") {
        Some(Provider::Claude)
    } else {
        None
    }
}

fn build_member_entry(
    team: &orchestration::team::Team,
    member: &orchestration::team::TeamMember,
    store_dir: &Path,
    config: &index::ReindexConfig,
) -> BroRosterEntry {
    let brofile = orchestration::brofile::resolve_brofile(
        &member.brofile,
        store_dir,
        team.project_dir.as_deref(),
    );
    let provider = brofile.as_ref().map(|b| b.provider);
    let session_id = member
        .session_id
        .as_ref()
        .filter(|s| s.as_str() != "pending")
        .cloned();
    let jsonl_path = session_id
        .as_deref()
        .and_then(|sid| index::find_session_file(sid, &config.roots, config.codex_root.as_deref()))
        .map(|p| p.to_string_lossy().into_owned());
    BroRosterEntry {
        bro: member.name.clone(),
        bro_selector: format!("{}::{}", team.name, member.name),
        team: team.name.clone(),
        provider: provider
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into()),
        session_id,
        jsonl_path,
        brofile: member.brofile.clone(),
        model: brofile.and_then(|b| b.model),
    }
}

fn roster_entry_key(entry: &BroRosterEntry) -> String {
    if let Some(ref sid) = entry.session_id {
        format!("session::{sid}")
    } else {
        format!("member::{}", entry.bro_selector)
    }
}

async fn roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<RosterQuery>,
) -> Result<axum::Json<Vec<BroRosterEntry>>, axum::http::StatusCode> {
    let store_dir = state.store_dir.clone();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();

    let no_selectors =
        wanted_teams.is_empty() && wanted_bros.is_empty() && wanted_sessions.is_empty();

    let mut seen = std::collections::HashSet::new();
    let mut entries = Vec::new();

    // Team selectors — each contributes all members. Unknown teams are
    // skipped silently; the empty roster speaks for itself at the CLI layer.
    for tn in &wanted_teams {
        if let Some(team) = orchestration::team::load_team(tn, &store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Bro selectors — include every match across all teams (deduped by team::bro).
    if !wanted_bros.is_empty() {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                if !wanted_bros.iter().any(|b| b == &member.name) {
                    continue;
                }
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    // Session selectors — synthetic adhoc lanes.
    for sid in &wanted_sessions {
        let key = format!("session::{sid}");
        if !seen.insert(key) {
            continue;
        }
        let path = index::find_session_file(sid, &config.roots, config.codex_root.as_deref());
        let provider = path.as_deref().and_then(infer_provider_from_path);
        entries.push(BroRosterEntry {
            bro: sid.chars().take(8).collect(),
            bro_selector: sid.clone(),
            team: "adhoc".into(),
            provider: provider
                .map(|p| p.to_string())
                .unwrap_or_else(|| "unknown".into()),
            session_id: Some(sid.clone()),
            jsonl_path: path.map(|p| p.to_string_lossy().into_owned()),
            brofile: String::new(),
            model: None,
        });
    }

    // No selectors → full roster across every team (legacy default).
    if no_selectors {
        for team in orchestration::team::load_all_teams(&store_dir) {
            for member in &team.members {
                let candidate = build_member_entry(&team, member, &store_dir, &config);
                let key = roster_entry_key(&candidate);
                if !seen.insert(key) {
                    continue;
                }
                entries.push(candidate);
            }
        }
    }

    if !wanted_providers.is_empty() {
        entries.retain(|e| {
            e.provider
                .parse::<Provider>()
                .ok()
                .map(|p| wanted_providers.contains(&p))
                .unwrap_or(false)
        });
    }

    Ok(axum::Json(entries))
}

// ---------------------------------------------------------------------------
// Tail SSE endpoint (outside MCP)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TailQuery {
    /// Comma-separated team names — union of members. Accepts legacy `team=`.
    #[serde(default, alias = "team")]
    teams: Option<String>,
    /// Comma-separated bro names. Accepts legacy `bro=`.
    #[serde(default, alias = "bro")]
    bros: Option<String>,
    /// Comma-separated session IDs — matches events by their task's session_id.
    #[serde(default, alias = "session")]
    sessions: Option<String>,
    /// Comma-separated provider names. Accepts legacy `provider=`.
    #[serde(default, alias = "provider")]
    providers: Option<String>,
}

async fn tail_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<TailQuery>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let mut rx = state.tail_tx.subscribe();
    let config = state.idx.read().reindex_config();

    let wanted_teams = split_csv(&query.teams);
    let wanted_bros = split_csv(&query.bros);
    let wanted_sessions = split_csv(&query.sessions);
    let wanted_providers: Vec<Provider> = split_csv(&query.providers)
        .iter()
        .filter_map(|p| p.parse::<Provider>().ok())
        .collect();
    let no_selectors = wanted_teams.is_empty()
        && wanted_bros.is_empty()
        && wanted_sessions.is_empty()
        && wanted_providers.is_empty();
    let store_dir = state.store_dir.clone();

    let stream = async_stream::stream! {
        loop {
            match rx.recv().await {
                Ok(event) => {
                    let tid = event.task_id();
                    let (task_provider, task_session_id) = {
                        let store = state.task_store.read();
                        store.get(tid)
                            .map(|t| {
                                let inner = t.inner.lock();
                                (Some(inner.provider), Some(inner.session_id.clone()))
                            })
                            .unwrap_or((None, None))
                    };
                    let bro_ref = orchestration::team::find_bro_ref_for_task(tid, &store_dir);

                    // Provider is a filter that applies on top of the selector
                    // union. Bros/sessions/teams are OR'd together: match ANY
                    // specified selector across them; a category being empty
                    // means it contributes no matches (but also doesn't reject).
                    let provider_ok = wanted_providers.is_empty()
                        || task_provider.map(|p| wanted_providers.contains(&p)).unwrap_or(false);
                    let selectors_specified = !wanted_bros.is_empty()
                        || !wanted_sessions.is_empty()
                        || !wanted_teams.is_empty();
                    let selector_match = if !selectors_specified {
                        true
                    } else {
                        let bro_m = bro_ref.as_ref()
                            .map(|r| {
                                let selector = format!("{}::{}", r.team_name, r.member_name);
                                wanted_bros.iter().any(|w| w == &r.member_name || w == &selector)
                            })
                            .unwrap_or(false);
                        let session_m = task_session_id.as_deref()
                            .map(|s| wanted_sessions.iter().any(|w| w == s))
                            .unwrap_or(false);
                        let team_m = wanted_teams.iter().any(|tn| {
                            orchestration::team::load_team(tn, &store_dir)
                                .map(|team| team.members.iter()
                                    .any(|m| m.task_history.iter().any(|id| id == tid)))
                                .unwrap_or(false)
                        });
                        bro_m || session_m || team_m
                    };
                    if !(no_selectors || (provider_ok && selector_match)) {
                        continue;
                    }

                    let mut evt_json = serde_json::to_value(&event).unwrap_or_default();
                    if let Some(ref bro_ref) = bro_ref {
                        evt_json["bro_name"] = Value::String(bro_ref.member_name.clone());
                        evt_json["bro_selector"] =
                            Value::String(format!("{}::{}", bro_ref.team_name, bro_ref.member_name));
                        evt_json["team_name"] = Value::String(bro_ref.team_name.clone());
                    }
                    if let Some(ref sid) = task_session_id {
                        if sid.as_str() != "pending" {
                            evt_json["session_id"] = Value::String(sid.clone());
                            if let Some(path) = index::find_session_file(
                                sid,
                                &config.roots,
                                config.codex_root.as_deref(),
                            ) {
                                evt_json["jsonl_path"] =
                                    Value::String(path.to_string_lossy().into_owned());
                            }
                        }
                    }
                    let data = serde_json::to_string(&evt_json).unwrap_or_default();
                    yield Ok(Event::default().data(data));
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!("tail subscriber lagged by {n} events");
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    };

    Sse::new(stream)
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    let migrated = util::migrate_legacy_defaults(&home)?;

    // Logging
    let log_dir = util::blackbox_log_dir(&home);
    std::fs::create_dir_all(&log_dir).expect("failed to create log directory");
    let file_appender = tracing_appender::rolling::Builder::new()
        .max_log_files(3)
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("blackbox")
        .filename_suffix("log")
        .build(&log_dir)
        .expect("failed to create log appender");

    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "blackbox=info".into());

    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    tracing_subscriber::registry()
        .with(env_filter)
        .with(tracing_subscriber::fmt::layer().with_writer(io::stderr))
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(file_appender)
                .with_ansi(false),
        )
        .init();

    std::panic::set_hook(Box::new(|info| {
        tracing::error!("PANIC: {}", info);
    }));
    for msg in migrated {
        tracing::info!("migrated legacy blackbox path: {msg}");
    }

    // Transcript index roots
    let roots: Vec<(String, PathBuf)> = if let Ok(val) = std::env::var("TRANSCRIPT_SEARCH_ROOTS") {
        val.split(',')
            .filter_map(|entry| {
                let (name, path) = entry.split_once('=')?;
                let expanded = if path.starts_with('~') {
                    home.join(&path[2..])
                } else {
                    PathBuf::from(path)
                };
                Some((name.to_string(), expanded))
            })
            .collect()
    } else {
        let mut found = vec![("claude".to_string(), home.join(".claude"))];
        if let Ok(entries) = std::fs::read_dir(&home) {
            let mut extras: Vec<(String, PathBuf)> = entries
                .filter_map(|e| e.ok())
                .filter(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.starts_with(".claude-")
                        && !name.contains("shared")
                        && e.path().join("projects").exists()
                })
                .map(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    let label = name.trim_start_matches(".claude-").to_string();
                    (label, e.path())
                })
                .collect();
            extras.sort_by(|a, b| a.0.cmp(&b.0));
            found.extend(extras);
        }
        found
    };

    let codex_root = std::env::var("TRANSCRIPT_SEARCH_CODEX_ROOT")
        .ok()
        .map(PathBuf::from)
        .or_else(|| {
            let default = home.join(".codex");
            if default.join("sessions").exists() {
                Some(default)
            } else {
                None
            }
        });

    let index_path = util::blackbox_index_path(&home);

    tracing::info!(
        "Roots: {:?}",
        roots
            .iter()
            .map(|(n, p)| format!("{n}={}", p.display()))
            .collect::<Vec<_>>()
    );
    if let Some(ref cr) = codex_root {
        tracing::info!("Codex root: {}", cr.display());
    }
    tracing::info!("Index path: {}", index_path.display());

    let idx = TranscriptIndex::open_or_create(&index_path, roots, codex_root)?;

    let kb_path = util::blackbox_knowledge_path(&home);
    let mut kb = Knowledge::open(&kb_path)?;
    tracing::info!("Knowledge store: {}", kb_path.display());

    // Sync the auto-generated tool reference into the knowledge store
    // so every agent's global memory picks up the current tool surface
    // on the next render. Idempotent: no-op when content is unchanged.
    match tool_docs::sync_into_knowledge(&mut kb) {
        Ok(r) if r.wrote => tracing::info!("Tool reference synced ({} bytes)", r.bytes),
        Ok(_) => tracing::debug!("Tool reference already up to date"),
        Err(e) => tracing::warn!("Tool reference sync failed: {e:#}"),
    }

    // Register blackbox in each installed provider's MCP config so that
    // every `{provider} ...` invocation (dispatched bros or interactive
    // sessions) sees the daemon without requiring user-managed config.
    // Resolves the "subprocessed bros don't see bbox tools" gap
    // discovered in the self-test pass.
    let bbox_port: u16 = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7264);
    let bbox_url = format!("http://127.0.0.1:{bbox_port}/mcp");
    let bbox_mcp_name = util::blackbox_mcp_name();
    // Export for provider arg-builders so they can inject `--mcp-config`
    // etc. at dispatch time — ensures dispatched subprocesses see
    // blackbox regardless of which config file their CLI inherits.
    std::env::set_var("BLACKBOX_MCP_URL", &bbox_url);
    std::env::set_var("BLACKBOX_MCP_NAME", &bbox_mcp_name);
    let report = orchestration::mcp::self_register_blackbox(&bbox_mcp_name, &bbox_url);
    tracing::info!(
        "blackbox MCP self-registration (name={}): {}",
        bbox_mcp_name,
        report.summary()
    );
    for (p, outcome) in &report.per_provider {
        if let orchestration::mcp::SelfRegisterOutcome::Error { detail } = outcome {
            tracing::warn!("self-register {p}: {detail}");
        }
    }

    // Sweep orphaned Gemini policy tempfiles from crashed/force-killed
    // dispatches. Files younger than 24h are kept in case they belong
    // to live tasks.
    match orchestration::mcp::sweep_stale_gemini_policies(24) {
        Ok(n) if n > 0 => tracing::info!("swept {n} stale gemini policy file(s)"),
        Ok(_) => {}
        Err(e) => tracing::debug!("gemini policy sweep: {e:#}"),
    }

    let th_path = util::blackbox_threads_path(&home);
    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());

    let notes_path = util::blackbox_notes_path(&home);
    let notes_store = Notes::open(&notes_path)?;
    tracing::info!("Notes store: {}", notes_path.display());

    let packets_dir = util::blackbox_packets_dir(&home);
    let packets_store = Packets::open(&packets_dir)?;
    tracing::info!("Packets store: {}", packets_dir.join("packets").display());

    // Orchestration state
    let store_dir = PathBuf::from(
        std::env::var("BRO_STORE")
            .unwrap_or_else(|_| util::bro_home_dir(&home).to_string_lossy().to_string()),
    );
    let task_ttl = std::env::var("BRO_TASK_TTL_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(86_400_000u64);
    let task_store = TaskStore::load(&store_dir, task_ttl);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);

    // Spawn background reindex thread
    let reindex_interval = std::env::var("BLACKBOX_REINDEX_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(120);
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );

    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        kb: RwLock::new(kb),
        threads: RwLock::new(th),
        notes: RwLock::new(notes_store),
        packets: RwLock::new(packets_store),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx: tail_tx.clone(),
        store_dir: store_dir.clone(),
    });

    // MCP service
    let port: u16 = std::env::var("BBOX_PORT")
        .or_else(|_| std::env::var("BRO_PORT"))
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(7264);

    let ct = CancellationToken::new();
    let config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_stateful_mode(true);

    let shared_for_mcp = shared.clone();
    let mcp_service: StreamableHttpService<BlackboxServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            Default::default(),
            config,
        );

    let app = axum::Router::new()
        .route("/tail", axum::routing::get(tail_handler))
        .route("/roster", axum::routing::get(roster_handler))
        .with_state(shared.clone())
        .nest_service("/mcp", mcp_service);

    let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}")).await?;
    tracing::info!("blackboxd listening on http://127.0.0.1:{port}/mcp");

    axum::serve(listener, app)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    // Persist tasks on shutdown
    shared.task_store.read().persist(&store_dir);
    tracing::info!("blackboxd shut down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        let index = TranscriptIndex::open_or_create(
            &tmp.path().join("index"),
            Vec::new(),
            None,
        )
        .unwrap();
        let kb = Knowledge::open(&tmp.path().join("knowledge.json")).unwrap();
        let threads = Threads::open(&tmp.path().join("threads.json")).unwrap();
        let notes = Notes::open(&tmp.path().join("notes.json")).unwrap();
        let packets = Packets::open(tmp.path()).unwrap();
        let (tail_tx, _) = broadcast::channel::<TailEvent>(16);
        let state = Arc::new(SharedState {
            idx: RwLock::new(index),
            kb: RwLock::new(kb),
            threads: RwLock::new(threads),
            notes: RwLock::new(notes),
            packets: RwLock::new(packets),
            task_store: Arc::new(RwLock::new(TaskStore::new())),
            tail_tx,
            store_dir: tmp.path().join("bro"),
        });
        BlackboxServer::new(state)
    }

    fn save_test_brofile(tmp: &tempfile::TempDir, name: &str) {
        orchestration::brofile::save_brofile(
            &orchestration::brofile::Brofile {
                name: name.to_string(),
                provider: Provider::Gemini,
                account: None,
                lens: None,
                model: None,
                effort: None,
                filters: None,
            },
            "global",
            &tmp.path().join("bro"),
            None,
        );
    }

    #[test]
    fn resolve_resume_target_rejects_ambiguous_bro_names_across_live_teams() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        save_test_brofile(&tmp, "reviewer");

        for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
            orchestration::team::save_team(
                &orchestration::team::Team {
                    name: team_name.to_string(),
                    teamplate: "review".into(),
                    members: vec![orchestration::team::TeamMember {
                        name: "reviewer".into(),
                        brofile: "reviewer".into(),
                        session_id: Some(session_id.into()),
                        task_history: vec![],
                    }],
                    project_dir: None,
                    created_at: 0,
                },
                &tmp.path().join("bro"),
            );
        }

        let err = server
            .resolve_resume_target(Some("reviewer"), None, None, None)
            .unwrap_err();
        assert!(err.contains("Ambiguous bro name: reviewer"));
        assert!(err.contains("red"));
        assert!(err.contains("blue"));
    }

    #[test]
    fn resolve_resume_target_accepts_scoped_team_bro_selector() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        save_test_brofile(&tmp, "reviewer");

        for (team_name, session_id) in [("red", "sid-red"), ("blue", "sid-blue")] {
            orchestration::team::save_team(
                &orchestration::team::Team {
                    name: team_name.to_string(),
                    teamplate: "review".into(),
                    members: vec![orchestration::team::TeamMember {
                        name: "reviewer".into(),
                        brofile: "reviewer".into(),
                        session_id: Some(session_id.into()),
                        task_history: vec![],
                    }],
                    project_dir: Some(format!("/tmp/{team_name}")),
                    created_at: 0,
                },
                &tmp.path().join("bro"),
            );
        }

        let (provider, session_id, _lens, _opts, _env, cwd, _filters) = server
            .resolve_resume_target(Some("blue::reviewer"), None, None, None)
            .unwrap();
        assert_eq!(provider, Provider::Gemini);
        assert_eq!(session_id, "sid-blue");
        assert_eq!(cwd.as_deref(), Some("/tmp/blue"));
    }
}

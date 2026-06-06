//! The transport-agnostic tool-calling loop.
//!
//! Two entry modes, both built on one [`Session`]:
//!
//! - **One-shot** (default): resolve a single prompt, run one user turn to
//!   completion, emit `result`, persist, exit. This is how the daemon dispatches
//!   today.
//! - **Bidirectional** (`--input-format stream-json`): keep a persistent session
//!   alive, reading successive user-turn messages and `control_request`s
//!   (interrupt, set_model, …) from stdin as NDJSON, and `/compact` as an
//!   in-stream slash command. This is the fleet-cockpit control plane
//!   (design/fleet-tui/fleet-tui.md §2). Wire shapes follow the Claude Agent
//!   SDK control protocol (hyperclaude SDK_PROTOCOL.md / NDJSON_FORMAT.md).
//!
//! The transport handles all wire differences; the loop and the stdout envelope
//! are identical across providers.

use crate::cli::Cli;
use crate::emit::{Emitter, EventCallback};
use crate::hooks::{Delivery, HookEngine, NudgeLedger};
use crate::lsp_baselines::LspBaselines;
use crate::mcp;
use crate::registry::{PinPolicy, Registry};
use crate::session::{SaveState, SessionStore};
use crate::transport::{self, StopReason, SystemPrompt, Transport, TransportKind, TurnOpts, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use bro_tools::{SafetyPolicy, ShellRun, Tool, ToolCx, ToolResult, builtin_tools};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::io::Read as _;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::{mpsc, watch};

// Output ceiling per turn. 32000 mirrors Claude Code's flat `max_tokens` for the
// Anthropic-compatible endpoints; with server-managed adaptive thinking the
// reasoning budget is not carved out of this, so output is no longer starved.
// Override with `BRO_HARNESS_MAX_TOKENS`.
const DEFAULT_MAX_TOKENS: u32 = 32000;
/// Hard backstop on loop iterations *per user turn*; the daemon's supervision is
/// the outer guard. Override with `BRO_HARNESS_MAX_TURNS`.
const DEFAULT_MAX_TURNS: u64 = 50;

fn builtin_tools_for_mode(fleet: bool) -> Vec<Arc<dyn Tool>> {
    let mut tools = builtin_tools();
    if fleet {
        tools.retain(|tool| {
            !matches!(
                tool.name(),
                "shell_run" | "shell_poll" | "shell_kill" | "shell_list"
            )
        });
        tools.push(Arc::new(FleetShellRun));
    }
    tools
}

struct FleetShellRun;

#[async_trait]
impl Tool for FleetShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }

    fn description(&self) -> &str {
        "Run a shell command in fleet mode as a harness-local Promise. Starts immediately and returns {promise_id,state,running,next_step}; use promise_wait, promise_status, promise_when_all, promise_when_any, or promise_cancel for lifecycle. Completion automatically injects a hidden HARNESS_EVENT turn unless a terminal wait already returned the result. The blocking/yield-poll shell_run path and shell_poll sessions are intentionally unavailable in fleet mode."
    }

    fn input_schema(&self) -> Value {
        let mut schema = ShellRun.input_schema();
        if let Some(props) = schema.get_mut("properties").and_then(Value::as_object_mut) {
            props.remove("mode");
            props.remove("yield_time_ms");
        }
        schema
    }

    async fn call(&self, mut input: Value, cx: &ToolCx) -> ToolResult {
        let Some(obj) = input.as_object_mut() else {
            return ToolResult::Error("bad input: expected object".into());
        };
        obj.insert("mode".into(), Value::String("promise".into()));
        obj.remove("yield_time_ms");
        ShellRun.call(input, cx).await
    }
}

/// Marker injected as a tool_result when a tool dispatch is interrupted, so the
/// transport buffer stays valid (every tool_use gets a matching result).
const INTERRUPTED_TOOL_RESULT: &str = "[Request interrupted by user]";

/// Entry point. Branches one-shot vs. bidirectional on `--input-format`.
pub async fn run(cli: Cli) -> Result<()> {
    run_with_emitter(cli, None, None).await
}

pub async fn run_with_event_callback(cli: Cli, callback: EventCallback) -> Result<()> {
    run_with_emitter(cli, Some(callback), None).await
}

#[derive(Debug)]
pub enum SessionInput {
    User(String),
    Control {
        subtype: String,
        request_id: Option<String>,
        raw: Value,
    },
}

pub type SessionInputSender = mpsc::UnboundedSender<SessionInput>;
pub type SessionInputReceiver = mpsc::UnboundedReceiver<SessionInput>;

pub fn session_input_channel() -> (SessionInputSender, SessionInputReceiver) {
    mpsc::unbounded_channel()
}

pub async fn run_with_event_callback_and_input(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: EventCallback,
) -> Result<()> {
    run_with_event_callback_and_input_mcp(cli, input_rx, callback, None).await
}

pub async fn run_with_event_callback_and_input_mcp(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: EventCallback,
    mcp_config: Option<mcp::McpConfig>,
) -> Result<()> {
    run_controlled_session(cli, input_rx, Some(callback), mcp_config).await
}

async fn run_with_emitter(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
) -> Result<()> {
    if cli.input_format.as_deref() == Some("stream-json") {
        return run_session(cli, callback, mcp_config).await;
    }

    // One-shot: a single prompt, one user turn, then persist and exit.
    let prompt = resolve_prompt(&cli)?;
    let mut session = Session::build(&cli, callback, mcp_config).await?;
    session.emitter.system_init();
    // A cancel channel that never fires — one-shot turns are not interruptible.
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    session
        .user_turn(&prompt, cancel_rx, Arc::new(StdMutex::new(VecDeque::new())))
        .await?;
    session.persist()?;
    Ok(())
}

fn make_emitter(session_id: String, callback: Option<EventCallback>) -> Emitter {
    match callback {
        Some(callback) => Emitter::with_callback(session_id, callback),
        None => Emitter::new(session_id),
    }
}

/// Bidirectional persistent session driven over stdin NDJSON.
async fn run_session(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
) -> Result<()> {
    let replay = cli.replay_user_messages;
    let mut session = Session::build(&cli, callback.clone(), mcp_config).await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();

    // The stdin reader runs as its own task so control messages (interrupt)
    // arrive while a turn is in flight. It owns a clone of the emitter purely to
    // honour `--replay-user-messages`.
    let input_rx = spawn_stdin_reader(replay, make_emitter(sid.clone(), callback.clone()));
    // A separate emitter for control responses emitted *during* a turn, when the
    // session's own emitter is borrowed by the running turn.
    let ctrl_emitter = make_emitter(sid, callback);

    // Steers that arrived mid-turn wait here for the next turn boundary.
    let mut pending: VecDeque<String> = VecDeque::new();
    // An initial `-p` prompt (if any) is the first user turn.
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }

    session_loop(&mut session, input_rx, &ctrl_emitter, pending).await?;
    session.persist()?;
    Ok(())
}

async fn run_controlled_session(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
) -> Result<()> {
    let mut session = Session::build(&cli, callback.clone(), mcp_config).await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();
    let ctrl_emitter = make_emitter(sid, callback);

    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }
    let input_rx = map_session_input(input_rx);

    session_loop_until_idle(&mut session, input_rx, &ctrl_emitter, pending).await?;
    session.persist()?;
    Ok(())
}

fn map_session_input(mut external_rx: SessionInputReceiver) -> mpsc::UnboundedReceiver<Input> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(input) = external_rx.recv().await {
            if tx.send(to_input(input)).is_err() {
                break;
            }
        }
    });
    rx
}

fn to_input(input: SessionInput) -> Input {
    match input {
        SessionInput::User(text) => Input::User(text),
        SessionInput::Control {
            subtype,
            request_id,
            raw,
        } => Input::Control {
            subtype,
            req_id: request_id,
            raw,
        },
    }
}

fn queue_redirect_from_control(raw: &Value, inputs: &Arc<StdMutex<VecDeque<String>>>) {
    let prompt = raw["prompt"]
        .as_str()
        .or_else(|| raw["request"]["prompt"].as_str())
        .map(str::to_string);
    if let Some(prompt) = prompt
        && let Ok(mut inputs) = inputs.lock()
    {
        inputs.push_back(prompt);
    }
}

/// The core bidirectional loop, factored out of `run_session` so it can be
/// driven by an injected input channel in tests (independent of real stdin /
/// HTTP). Persistence is the caller's responsibility.
async fn session_loop(
    session: &mut Session,
    mut input_rx: mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    mut pending: VecDeque<String>,
) -> Result<()> {
    loop {
        let prompt = match pending.pop_front() {
            Some(p) => p,
            None => {
                if let Some(event) = session.promise_completion_event_prompt() {
                    event
                } else {
                    let promise_notify = session.promise_notifier();
                    tokio::select! {
                        maybe = input_rx.recv() => match maybe {
                            Some(Input::User(p)) => p,
                            Some(Input::Control {
                                subtype,
                                req_id,
                                raw,
                            }) => {
                                // Control while idle: apply any mutation, ack success.
                                session.apply_control(&subtype, &raw);
                                ctrl_emitter.control_response_success(req_id.as_deref());
                                continue;
                            }
                            None => break, // stdin closed and nothing pending
                        },
                        _ = promise_notify.notified() => {
                            match session.promise_completion_event_prompt() {
                                Some(event) => event,
                                None => continue,
                            }
                        }
                    }
                }
            }
        };

        // `/compact` is an in-stream slash command, not a model turn.
        if prompt.trim() == "/compact" {
            if let Err(e) = session.compact_manual().await {
                tracing::warn!("manual /compact failed: {e:#}");
            }
            continue;
        }

        // Run the turn while concurrently watching stdin for an interrupt
        // (cancels the turn) or a steer. User steers received during a
        // tool-calling turn are injected at the next model-call boundary inside
        // that same turn, after any outstanding tool results are pushed.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut stdin_closed = false;
        let mut deferred: Vec<(String, Value)> = Vec::new();
        let mid_turn_user_inputs: Arc<StdMutex<VecDeque<String>>> =
            Arc::new(StdMutex::new(VecDeque::new()));
        {
            let turn = session.user_turn(&prompt, cancel_rx, mid_turn_user_inputs.clone());
            tokio::pin!(turn);
            loop {
                tokio::select! {
                    biased;
                    res = &mut turn => {
                        if let Err(e) = res {
                            tracing::error!("turn failed: {e:#}");
                        }
                        break;
                    }
                    maybe = input_rx.recv() => match maybe {
                        Some(Input::Control { subtype, req_id, raw }) if subtype == "interrupt" => {
                            queue_redirect_from_control(&raw, &mid_turn_user_inputs);
                            let _ = cancel_tx.send(true);
                            ctrl_emitter.control_response_success(req_id.as_deref());
                        }
                        Some(Input::User(p)) => {
                            if let Ok(mut inputs) = mid_turn_user_inputs.lock() {
                                inputs.push_back(p);
                            }
                        }
                        Some(Input::Control { subtype, req_id, raw }) => {
                            // Non-interrupt controls (set_model, …) ack now and
                            // apply at the turn boundary, when self is free.
                            ctrl_emitter.control_response_success(req_id.as_deref());
                            deferred.push((subtype, raw));
                        }
                        None => {
                            let _ = cancel_tx.send(true);
                            stdin_closed = true;
                        }
                    }
                }
            }
        }
        // The turn (and its &mut self borrow) is done — apply deferred controls.
        for (subtype, raw) in deferred {
            session.apply_control(&subtype, &raw);
        }
        if let Ok(mut inputs) = mid_turn_user_inputs.lock() {
            while let Some(p) = inputs.pop_back() {
                pending.push_front(p);
            }
        }
        // Persist after every turn, not just at clean session exit. A bidi
        // session is routinely killed (SIGTERM on fleet stop / cockpit close)
        // before the end-of-`run_session` persist runs; without this, every
        // completed turn is lost and a `--resume` finds no session file and
        // starts cold. Per-turn persistence bounds the loss to at most the
        // single in-flight turn.
        if let Err(e) = session.persist() {
            tracing::warn!("failed to persist session after turn: {e:#}");
        }
        if stdin_closed && pending.is_empty() {
            break;
        }
    }
    Ok(())
}

async fn session_loop_until_idle(
    session: &mut Session,
    mut input_rx: mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    mut pending: VecDeque<String>,
) -> Result<()> {
    loop {
        let prompt = match pending.pop_front() {
            Some(p) => p,
            None => match input_rx.try_recv() {
                Ok(Input::User(p)) => p,
                Ok(Input::Control {
                    subtype,
                    req_id,
                    raw,
                }) => {
                    session.apply_control(&subtype, &raw);
                    ctrl_emitter.control_response_success(req_id.as_deref());
                    continue;
                }
                Err(_) => break,
            },
        };

        run_prompt_with_controls(session, &mut input_rx, ctrl_emitter, &mut pending, prompt)
            .await?;
        if let Err(e) = session.persist() {
            tracing::warn!("failed to persist session after controlled turn: {e:#}");
        }
    }
    Ok(())
}

async fn run_prompt_with_controls(
    session: &mut Session,
    input_rx: &mut mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    pending: &mut VecDeque<String>,
    prompt: String,
) -> Result<()> {
    if prompt.trim() == "/compact" {
        if let Err(e) = session.compact_manual().await {
            tracing::warn!("manual /compact failed: {e:#}");
        }
        return Ok(());
    }

    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut deferred: Vec<(String, Value)> = Vec::new();
    let mid_turn_user_inputs: Arc<StdMutex<VecDeque<String>>> =
        Arc::new(StdMutex::new(VecDeque::new()));
    let turn_result = {
        let turn = session.user_turn(&prompt, cancel_rx, mid_turn_user_inputs.clone());
        tokio::pin!(turn);
        loop {
            tokio::select! {
                biased;
                res = &mut turn => break res,
                maybe = input_rx.recv() => match maybe {
                    Some(Input::Control { subtype, req_id, raw }) if subtype == "interrupt" => {
                        queue_redirect_from_control(&raw, &mid_turn_user_inputs);
                        let _ = cancel_tx.send(true);
                        ctrl_emitter.control_response_success(req_id.as_deref());
                    }
                    Some(Input::User(p)) => {
                        if let Ok(mut inputs) = mid_turn_user_inputs.lock() {
                            inputs.push_back(p);
                        }
                    }
                    Some(Input::Control { subtype, req_id, raw }) => {
                        ctrl_emitter.control_response_success(req_id.as_deref());
                        deferred.push((subtype, raw));
                    }
                    None => {
                        let _ = cancel_tx.send(true);
                    }
                }
            }
        }
    };
    // A failed turn must not be swallowed. Surface it as a terminal `result`
    // event with `is_error: true` (captured in the transcript and ingested by
    // the daemon, which maps it to a failed task with the message preserved) in
    // addition to the stderr log. Without this the controlled-session loop
    // returns Ok and the dispatch looks like a silent successful completion
    // with no result — the silent-completion hole tracked in gap-32113fd4.
    if let Err(e) = turn_result {
        tracing::error!("turn failed: {e:#}");
        session.emitter.result_error(&format!("{e:#}"), session.turns);
    }
    for (subtype, raw) in deferred {
        session.apply_control(&subtype, &raw);
    }
    if let Ok(mut inputs) = mid_turn_user_inputs.lock() {
        while let Some(p) = inputs.pop_back() {
            pending.push_front(p);
        }
    }
    Ok(())
}

/// Persistent per-dispatch state, shared by both entry modes.
struct Session {
    tx: Box<dyn Transport>,
    reg: Registry,
    cx: ToolCx,
    hooks: HookEngine,
    emitter: Emitter,
    base_opts: TurnOpts,
    /// Caller-supplied system text (None ⇒ provider defaults).
    system: Option<String>,
    max_turns: u64,
    compaction: crate::compaction::CompactionPolicy,
    compact_threshold: Option<u64>,
    /// Tool-result spill threshold in bytes (0 ⇒ disabled) and the dump dir.
    tool_result_cap: usize,
    dump_dir: std::path::PathBuf,
    store: SessionStore,
    prior_side: Value,
    todos: Arc<std::sync::Mutex<bro_tools::TodoList>>,
    kv: Arc<crate::capabilities::KvStore>,
    /// Cross-turn diagnostics baselines: per-file `{sha256, version,
    /// diagnostics}` snapshots from the most recent analyzer pass, so a
    /// future differ can surface only NEW/CHANGED findings on the next edit.
    /// Seeded from `side["lsp_baselines"]` on build; flushed in `persist()`.
    lsp_baselines: LspBaselines,
    /// Loop-lived LSP pool and open-document handles. The pool keeps warm
    /// language-server sessions across edits; the document map lets the spine
    /// apply didChange instead of reopening files on every mutation.
    lsp_pool: bro_lsp::SessionPool,
    lsp_documents: BTreeMap<String, bro_lsp::OpenDocument>,
    // Mutable accumulators carried across user turns.
    total_usage: Usage,
    turns: u64,
    last_prompt_tokens: u64,
    /// Estimated tokens appended to the transport buffer since `last_prompt_tokens`
    /// was last observed — tool results, mid-turn inputs, and the new user
    /// message. Added to `last_prompt_tokens` for the proactive compaction
    /// trigger, so an appended item that would push the *next* request over the
    /// window triggers compaction before it is sent (rather than reacting one
    /// step late, or relying on the overflow safety net). Mirrors codex's
    /// `get_total_token_usage` = last observed total + estimate of items after
    /// the last model turn (`context_manager/history.rs`). Reset to 0 on each
    /// model call (the sent input is then measured) and on compaction.
    pending_input_estimate: u64,
    /// Volatile system-tail nudge to surface on the upcoming model call.
    tail_nudge: Option<String>,
}

impl Session {
    async fn build(
        cli: &Cli,
        callback: Option<EventCallback>,
        injected_mcp: Option<mcp::McpConfig>,
    ) -> Result<Self> {
        if let Some(fmt) = cli.output_format.as_deref()
            && fmt != "stream-json"
        {
            anyhow::bail!("unsupported --output-format {fmt}; only stream-json");
        }

        let max_tokens = env_u32("BRO_HARNESS_MAX_TOKENS").unwrap_or(DEFAULT_MAX_TOKENS);
        let max_turns = env_u64("BRO_HARNESS_MAX_TURNS").unwrap_or(DEFAULT_MAX_TURNS);
        let web_search = std::env::var("BRO_HARNESS_WEB_SEARCH")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
            .unwrap_or(true);

        // Three-state --system-prompt:
        //   non-empty ⇒ explicit override, used verbatim;
        //   ""        ⇒ explicit suppress (no system prompt) — Codex's
        //               project_doc_max_bytes=0 / AGENTS-omitting overlay analog;
        //   absent    ⇒ not overridden ⇒ Codex-style AGENTS.md overlay
        //               (global $CODEX_HOME/AGENTS.md + repo AGENTS.md, project
        //               scope). Falls back to None when no AGENTS docs exist.
        // Per-session working directory: explicit `--cwd` (the daemon's
        // dispatch cwd, passed instead of mutating the process cwd) or the
        // process cwd for the standalone binary. All file/shell tools and
        // project-doc discovery resolve against this root, so concurrent
        // in-process sessions never collide (harness-daemon-boundary.md §3).
        let root = match cli.cwd.as_deref() {
            Some(c) => std::fs::canonicalize(c).unwrap_or_else(|_| std::path::PathBuf::from(c)),
            None => std::env::current_dir().context("cwd")?,
        };

        let system = match cli.system_prompt.as_deref() {
            Some("") => None,
            Some(s) => Some(s.to_string()),
            None => crate::project_doc::discover(&root),
        };

        let kind = TransportKind::from_env();
        let mut tx = transport::build_transport(kind).await?;

        let store = SessionStore::open(cli.session_id.as_deref(), cli.resume.as_deref())?;
        // Hand the transport the stable session id, so it can populate the
        // codex-style `session-id` header + `prompt_cache_key` (vs a random
        // per-request id).
        tx.set_session_id(store.id.clone());
        let restored_model = store.restored.as_ref().and_then(|r| r.model.clone());
        // Loop-level side cells restored from a prior turn. Each cell
        // deserializes its own slot tolerantly (absent/garbage → empty).
        let prior_side = store
            .restored
            .as_ref()
            .map(|r| r.side.clone())
            .unwrap_or(Value::Null);
        let todos = Arc::new(std::sync::Mutex::new(bro_tools::TodoList::from_side(
            prior_side.get("todos").unwrap_or(&Value::Null),
        )));
        let kv = Arc::new(crate::capabilities::KvStore::from_side(
            prior_side.get("narf_kv").unwrap_or(&Value::Null),
        ));
        let hooks = HookEngine::from_env(NudgeLedger::from_side(
            prior_side.get("nudges").unwrap_or(&Value::Null),
        ));
        let lsp_baselines =
            LspBaselines::from_side(prior_side.get("lsp_baselines").unwrap_or(&Value::Null));
        if let Some(r) = &store.restored {
            if r.transport != tx.name() {
                anyhow::bail!(
                    "resume transport mismatch: session is '{}', harness is '{}'",
                    r.transport,
                    tx.name()
                );
            }
            tx.restore(r.snapshot.clone());
        }

        // On resume the daemon doesn't re-pass --model (implied by the session),
        // so fall back to the model persisted with the session.
        let model = cli
            .model
            .clone()
            .or(restored_model)
            .or_else(|| transport::session_var("ANTHROPIC_MODEL"))
            .or_else(|| std::env::var("BRO_HARNESS_MODEL").ok())
            .context(
                "no --model, no resumed session model, and no ANTHROPIC_MODEL/BRO_HARNESS_MODEL",
            )?;

        let edits = Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default()));
        let cx = ToolCx {
            root: root.clone(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(std::sync::Mutex::new(bro_tools::PromiseStore::default())),
            edits: edits.clone(),
            session_env: Arc::new(transport::session_env_snapshot()),
        };
        // The builtin `report` tool is harness-owned (it emits the cockpit's
        // status signal on the stream) and holds its own emitter handle. It is
        // registered always but only pinned in fleet (bidirectional) mode.
        let fleet = cli.input_format.as_deref() == Some("stream-json");
        let tool_filter =
            mcp::ToolFilter::from_csv(cli.deny_tools.as_deref(), cli.allow_tools.as_deref());
        let mut builtins = builtin_tools_for_mode(fleet);
        builtins.push(Arc::new(crate::report::ReportTool::new(make_emitter(
            store.id.clone(),
            callback.clone(),
        ))));
        let (mcp_tools, tool_placement) = match injected_mcp {
            Some(config) => {
                let tools = mcp::load_mcp_tools_from_config(&config, &tool_filter).await;
                (tools, config.tool_placement)
            }
            None => {
                let tools = mcp::load_mcp_tools(cli.mcp_config.as_deref(), &tool_filter).await;
                let placement = mcp::parse_tool_placement(cli.mcp_config.as_deref());
                (tools, placement)
            }
        };
        let (mcp_in_box, mcp_out_box) =
            mcp::split_mcp_tools_by_placement(&mcp_tools, &tool_placement);
        // Code-mode projects the full tool surface (builtins + all MCP) into the
        // exec/wait authorial surface, so capture every MCP tool regardless of
        // box placement before the in/out-box vecs are consumed below.
        let mcp_all_for_code_mode: Vec<Arc<dyn Tool>> =
            mcp_in_box.iter().chain(mcp_out_box.iter()).cloned().collect();
        // In-process capability bindings (harness-daemon-boundary.md §6): when the
        // daemon has installed corpus/atom/refactor impls, expose them as direct
        // trait-dispatch tools (corpus_search, atom_invoke, refactor_plan, KV
        // inspection). Empty (no-op) for the standalone binary, so those surfaces
        // fail closed by absence. Registered as builtins so the surface ToolFilter
        // still gates them. The authorial surface is now code-mode (below).
        let kv_cap: Arc<dyn bro_capabilities::KvCapability> = kv.clone();
        builtins.extend(crate::capabilities::capability_tools(Some(kv_cap)));
        // Code-mode (exec/wait) supersedes NARF as the authorial surface. The
        // callable set mirrors the flat surface — filtered builtins + capability
        // tools + all MCP — and a ToolCapability seam over that same set
        // dispatches a cell's nested tools.* (deny-filter honored; exec/wait are
        // excluded from the projected namespace so a cell cannot relaunch the box).
        let mut cm_callable: Vec<Arc<dyn Tool>> = builtins
            .iter()
            .filter(|t| tool_filter.permits(t.name()))
            .cloned()
            .collect();
        cm_callable.extend(mcp_all_for_code_mode);
        let cm_seam: Arc<dyn bro_capabilities::ToolCapability> = Arc::new(
            crate::capabilities::HostTools::new(cm_callable.clone(), cx.clone()),
        );
        builtins.extend(crate::code_mode::code_mode_tools(&cm_callable, cm_seam));
        let mut pin = PinPolicy::from_env();
        pin.also_pin(bro_code_mode::PUBLIC_TOOL_NAME);
        pin.also_pin(bro_code_mode::WAIT_TOOL_NAME);
        if fleet {
            pin.also_pin(crate::report::REPORT_TOOL);
        }
        let reg = Registry::new(builtins, mcp_out_box, &pin, &tool_filter);

        let base_opts = TurnOpts {
            model,
            max_tokens,
            system: SystemPrompt::default(),
            effort: cli.effort.clone(),
            web_search,
            service_tier: cli
                .service_tier
                .clone()
                .or_else(|| std::env::var("BRO_HARNESS_SERVICE_TIER").ok()),
        };

        let emitter = make_emitter(store.id.clone(), callback);
        let compaction = crate::compaction::CompactionPolicy::from_env();
        let compact_threshold = compaction.threshold(&base_opts.model);
        let tool_result_cap = crate::bound::cap_bytes();
        let dump_dir = crate::bound::dump_dir();

        Ok(Self {
            tx,
            reg,
            cx,
            hooks,
            emitter,
            base_opts,
            system,
            max_turns,
            compaction,
            compact_threshold,
            tool_result_cap,
            dump_dir,
            store,
            prior_side,
            todos,
            kv,
            lsp_baselines,
            lsp_pool: bro_lsp::SessionPool::new(bro_lsp::LspConfig::default()),
            lsp_documents: BTreeMap::new(),
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
            pending_input_estimate: 0,
            tail_nudge: None,
        })
    }

    fn session_id(&self) -> &str {
        self.emitter.session_id()
    }

    /// Apply a mid-session control mutation. `interrupt` is handled by the
    /// caller (cancellation); everything else that mutates state lands here.
    fn apply_control(&mut self, subtype: &str, raw: &Value) {
        if subtype == "set_model"
            && let Some(m) = raw["model"]
                .as_str()
                .or_else(|| raw["request"]["model"].as_str())
        {
            self.base_opts.model = m.to_string();
            self.compact_threshold = self.compaction.threshold(m);
            tracing::info!(model = m, "set_model");
        }
        // set_max_thinking_tokens / others are accepted (acked) but not yet
        // wired to a runtime knob; they no-op rather than error.
    }

    /// Manual `/compact`: summarize-and-replace the prefix and emit a manual
    /// `compact_boundary`. A no-op (logged) when there isn't enough history.
    async fn compact_manual(&mut self) -> Result<()> {
        let tool_specs = self.reg.wire_specs();
        match self
            .tx
            .compact(
                self.compaction.params(),
                crate::compaction::COMPACTION_INSTRUCTION,
                &tool_specs,
                &self.base_opts,
            )
            .await?
        {
            Some(summary) => {
                self.emitter
                    .compact_boundary("manual", self.last_prompt_tokens, summary.len());
            }
            None => tracing::info!("manual /compact: nothing compactible yet"),
        }
        Ok(())
    }

    /// Run one user turn to completion (or until interrupted): push the user
    /// message, loop model-call → tool-dispatch until the model stops, emitting
    /// the stream-json envelope throughout, then emit `result`.
    ///
    /// `cancel` is observed at each await point. On interrupt: a cancelled model
    /// call leaves no assistant message (buffer stays valid); a cancelled tool
    /// dispatch pads the remaining tool_uses with an interrupted marker result
    /// so the buffer stays valid for the next turn.
    async fn user_turn(
        &mut self,
        prompt: &str,
        mut cancel: watch::Receiver<bool>,
        mid_turn_user_inputs: Arc<StdMutex<VecDeque<String>>>,
    ) -> Result<()> {
        self.push_user_text(prompt);

        let mut final_text = String::new();
        let mut turn_steps = 0u64;
        let mut last_model_stop: Option<StopReason> = None;
        let mut last_model_tool_call_count = 0usize;
        let mut last_tool_results: Vec<Value> = Vec::new();

        let break_reason = 'turn: loop {
            if turn_steps >= self.max_turns {
                tracing::warn!(max_turns = self.max_turns, "hit max turns; stopping");
                break "max_turns";
            }
            if *cancel.borrow() {
                break "cancelled";
            }
            if let Some(event) = self.promise_completion_event_prompt() {
                self.push_user_text(&event);
            }

            let tool_specs = self.reg.wire_specs();

            // Compact before composing when the projected next prompt crosses the
            // model's window threshold. "Projected" = last observed input plus an
            // estimate of items appended since (tool results, mid-turn inputs, the
            // new user message), so an appended item that would overflow the next
            // request triggers compaction *before* it's sent. Tools are forwarded
            // so the server-side compaction path (brodex) can faithfully process
            // tool-call history.
            let projected_tokens = self
                .last_prompt_tokens
                .saturating_add(self.pending_input_estimate);
            if let Some(thresh) = self.compact_threshold
                && projected_tokens > thresh
            {
                match self
                    .tx
                    .compact(
                        self.compaction.params(),
                        crate::compaction::COMPACTION_INSTRUCTION,
                        &tool_specs,
                        &self.base_opts,
                    )
                    .await
                {
                    Ok(Some(summary)) => {
                        tracing::info!(pre_tokens = projected_tokens, "compacted");
                        self.emitter
                            .compact_boundary("auto", projected_tokens, summary.len());
                        // The buffer was rewritten; its size will be re-measured
                        // by the upcoming call, so the appended-tail estimate no
                        // longer applies.
                        self.pending_input_estimate = 0;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("compaction failed: {e:#}"),
                }
            }
            let mut sys = compose_system(self.system.as_deref(), &self.reg);
            if let Some(t) = self.tail_nudge.take() {
                let v = sys.volatile.get_or_insert_with(String::new);
                if !v.is_empty() {
                    v.push('\n');
                }
                v.push_str(&t);
            }
            let opts = TurnOpts {
                system: sys,
                ..self.base_opts.clone()
            };

            // Run the model call, recovering once from a context-window
            // rejection by compacting and retrying. This is the reactive safety
            // net for the case the proactive threshold check above misses: a
            // single step (e.g. a large tool result) jumping over the window in
            // one shot. Without it, the typed `ContextWindowExceeded` would fail
            // the whole turn instead of self-healing.
            let mut overflow_compacted = false;
            let out = 'attempt: loop {
                let r = tokio::select! {
                    biased;
                    _ = cancel.changed() => {
                        break 'turn "cancelled";
                    }
                    r = self.tx.run_turn(&tool_specs, &opts, &self.emitter) => r,
                };
                match r {
                    Ok(out) => break 'attempt out,
                    Err(e)
                        if !overflow_compacted
                            && crate::transport::is_context_window_exceeded(&e) =>
                    {
                        overflow_compacted = true;
                        tracing::warn!("context window exceeded mid-turn; compacting and retrying");
                        match self
                            .tx
                            .compact(
                                self.compaction.params(),
                                crate::compaction::COMPACTION_INSTRUCTION,
                                &tool_specs,
                                &self.base_opts,
                            )
                            .await
                        {
                            Ok(Some(summary)) => self.emitter.compact_boundary(
                                "overflow",
                                self.last_prompt_tokens,
                                summary.len(),
                            ),
                            // Nothing compactible, or compaction itself failed:
                            // a retry would just re-overflow — surface the
                            // original error.
                            Ok(None) => return Err(e),
                            Err(ce) => {
                                tracing::warn!("overflow compaction failed: {ce:#}");
                                return Err(e);
                            }
                        }
                        // Loop to retry run_turn against the compacted buffer.
                    }
                    Err(e) => return Err(e),
                }
            };
            turn_steps += 1;
            self.turns += 1;
            self.total_usage.add(&out.usage);
            self.last_prompt_tokens = out.usage.total_input_tokens();
            // The just-sent input is now reflected in last_prompt_tokens; clear
            // the appended-tail estimate so it only counts items added afterward.
            self.pending_input_estimate = 0;
            last_model_stop = Some(out.stop.clone());
            last_model_tool_call_count = out.tool_calls.len();

            for n in self.hooks.on_assistant_turn(&out.text, &out.tool_calls) {
                if n.delivery == Delivery::SystemTail {
                    self.tail_nudge = Some(n.message);
                }
            }

            // Full assistant turn (thinking + text + tool_use) for the daemon
            // tail / fleet transcript; the daemon dedupes text against streamed
            // deltas. The thinking block is display-only — it is emitted here for
            // client rendering but never enters the transport replay buffer.
            let mut assistant_content: Vec<Value> = Vec::new();
            if !out.thinking.is_empty() {
                assistant_content.push(json!({"type": "thinking", "thinking": out.thinking}));
            }
            if !out.text.is_empty() {
                assistant_content.push(json!({"type": "text", "text": out.text}));
                final_text = out.text.clone();
            }
            for tc in &out.tool_calls {
                assistant_content.push(json!({
                    "type": "tool_use",
                    "id": tc.id,
                    "name": tc.name,
                    "input": tc.args,
                }));
            }
            if !assistant_content.is_empty() {
                self.emitter.assistant_message(assistant_content);
            }

            if out.stop != StopReason::ToolCalls || out.tool_calls.is_empty() {
                break if out.stop == StopReason::ToolCalls {
                    "tool_calls_empty"
                } else {
                    "model_stop"
                };
            }

            // Dispatch tool calls, interruptibly. On interrupt mid-dispatch, pad
            // every not-yet-resolved call with an interrupted marker so the
            // assistant(tool_use) message keeps a matching tool_result.
            let mut results: Vec<transport::ToolResult> = Vec::with_capacity(out.tool_calls.len());
            last_tool_results.clear();
            let mut interrupted = false;
            'dispatch: for tc in &out.tool_calls {
                tracing::info!(tool = %tc.name, "dispatch");
                tokio::select! {
                    biased;
                    _ = cancel.changed() => { interrupted = true; break 'dispatch; }
                    res = self.reg.dispatch(&tc.name, tc.args.clone(), &self.cx) => {
                        let (content, is_error) = res.into_content();
                        // Spill an oversized result to disk and inline a head +
                        // rider, uniformly across builtin and MCP tools (§2.3).
                        let content = crate::bound::bound_tool_result(
                            &tc.name,
                            content,
                            self.tool_result_cap,
                            &self.dump_dir,
                            &tc.id,
                        );
                        let mut result = transport::ToolResult {
                            id: tc.id.clone(),
                            content,
                            is_error,
                        };
                        self.append_window0_diagnostics(&mut result.content).await;
                        for n in self.hooks.on_tool_result(tc, &result) {
                            match n.delivery {
                                Delivery::Rider => result.content.push_str(&n.rider_block()),
                                Delivery::SystemTail => self.tail_nudge = Some(n.message),
                            }
                        }
                        last_tool_results.push(tool_result_trace(tc, &result));
                        results.push(result);
                    }
                }
            }
            if interrupted {
                let have: HashSet<String> = results.iter().map(|r| r.id.clone()).collect();
                for tc in &out.tool_calls {
                    if !have.contains(&tc.id) {
                        results.push(transport::ToolResult {
                            id: tc.id.clone(),
                            content: INTERRUPTED_TOOL_RESULT.to_string(),
                            is_error: true,
                        });
                        last_tool_results.push(json!({
                            "id": tc.id,
                            "name": tc.name,
                            "is_error": true,
                            "interrupted": true,
                        }));
                    }
                }
                self.emitter.tool_results(&results);
                self.pending_input_estimate = self
                    .pending_input_estimate
                    .saturating_add(est_tool_results(&results));
                self.tx.push_tool_results(results);
                break "interrupted_dispatch";
            }

            self.emitter.tool_results(&results);
            self.pending_input_estimate = self
                .pending_input_estimate
                .saturating_add(est_tool_results(&results));
            self.tx.push_tool_results(results);
            self.drain_mid_turn_user_inputs(&mid_turn_user_inputs);
            self.hooks.tick();
        };

        // An interrupted turn (cancelled model call, or cancelled tool dispatch)
        // leaves the buffer ending on a user-role message with no assistant
        // reply. Repair alternation now so the next turn — a steer, or a
        // `--resume` continuation — does not stack two user messages and 400.
        if matches!(break_reason, "cancelled" | "interrupted_dispatch") {
            self.tx.note_interrupted();
        }

        let turn_end = self.turn_end_diagnostics(
            break_reason,
            last_model_stop.as_ref(),
            last_model_tool_call_count,
            turn_steps,
            &last_tool_results,
            &final_text,
        );
        tracing::info!(turn_end = %turn_end, "turn ending");
        if turn_end["suspicious"].as_bool().unwrap_or(false) {
            tracing::warn!(turn_end = %turn_end, "suspicious turn end");
            self.emitter.turn_end_diagnostics(turn_end);
        }

        self.emitter
            .result(&final_text, &self.total_usage, self.turns, None);
        Ok(())
    }

    fn push_user_text(&mut self, prompt: &str) {
        self.tx.push_user_text(prompt);
        self.pending_input_estimate = self
            .pending_input_estimate
            .saturating_add(est_tokens(prompt));
        for n in self.hooks.on_user_turn(prompt) {
            if n.delivery == Delivery::SystemTail {
                self.tail_nudge = Some(n.message);
            }
        }
    }

    fn drain_mid_turn_user_inputs(&mut self, inputs: &Arc<StdMutex<VecDeque<String>>>) {
        let Ok(mut inputs) = inputs.lock() else {
            return;
        };
        while let Some(prompt) = inputs.pop_front() {
            if prompt.trim() == "/compact" {
                tracing::info!("deferring /compact received during active turn");
                continue;
            }
            self.push_user_text(&prompt);
        }
    }

    fn turn_end_diagnostics(
        &self,
        break_reason: &str,
        last_model_stop: Option<&StopReason>,
        last_model_tool_call_count: usize,
        turn_steps: u64,
        last_tool_results: &[Value],
        final_text: &str,
    ) -> Value {
        let shell_ids = self.cx.shell_sessions.lock().unwrap().ids();
        let promise_list = self.cx.promises.lock().unwrap().list();
        let promises = promise_summary(&promise_list);
        let running_promises = promises["running_count"].as_u64().unwrap_or(0);
        let last_tool_running = last_tool_results
            .iter()
            .any(|v| v["running"].as_bool() == Some(true));
        let outstanding_async = !shell_ids.is_empty() || running_promises > 0 || last_tool_running;

        // Empty-output stop: the model itself ended the turn (not max_turns /
        // cancel / interrupt) having produced no assistant text AND no tool
        // calls. This is the classic spurious-stop signature — the model
        // returned nothing and the turn silently terminated — which the
        // outstanding-async heuristic above does not catch. A `tool_calls_empty`
        // break is abnormal by construction (stop=tool_calls yet zero calls);
        // for it we flag regardless of text.
        let produced_text = !final_text.trim().is_empty();
        let model_ended = matches!(break_reason, "model_stop" | "tool_calls_empty");
        let empty_output_stop = model_ended && last_model_tool_call_count == 0 && !produced_text;

        let mut suspicion_reasons: Vec<&str> = Vec::new();
        if !shell_ids.is_empty() {
            suspicion_reasons.push("outstanding_shell_sessions");
        }
        if running_promises > 0 {
            suspicion_reasons.push("running_promises");
        }
        if last_tool_running {
            suspicion_reasons.push("last_tool_running");
        }
        if empty_output_stop {
            suspicion_reasons.push("empty_output_stop");
        }
        let suspicious = outstanding_async || empty_output_stop;

        json!({
            "break_reason": break_reason,
            "transport": self.tx.name(),
            "last_model_stop": stop_reason_label(last_model_stop),
            "last_model_tool_call_count": last_model_tool_call_count,
            "turn_steps": turn_steps,
            "harness_turns_total": self.turns,
            "final_text_len": final_text.len(),
            "produced_text": produced_text,
            "empty_output_stop": empty_output_stop,
            "outstanding_shell_sessions": {
                "count": shell_ids.len(),
                "ids": shell_ids,
            },
            "promises": promises,
            "last_tool_results": last_tool_results,
            "suspicion_reasons": suspicion_reasons,
            "suspicious": suspicious,
        })
    }

    fn promise_notifier(&self) -> Arc<tokio::sync::Notify> {
        self.cx.promises.lock().unwrap().notifier()
    }

    fn promise_completion_event_prompt(&mut self) -> Option<String> {
        let events = self.cx.promises.lock().unwrap().drain_completion_events();
        (!events.is_empty()).then(|| events.join("\n\n"))
    }

    /// Window-0 diagnostics seam: drain the per-dispatch edit sink, run the
    /// analyzer against the edited files, and append a diagnostics rider to the
    /// tool result `content` — synchronously, before control returns to the
    /// model. A no-op when no edits were recorded. A diagnostics failure is
    /// logged and swallowed: it must never break the dispatch loop.
    async fn append_window0_diagnostics(&mut self, content: &mut String) {
        let edits = self
            .cx
            .edits
            .lock()
            .map(|mut edits| edits.drain())
            .unwrap_or_default();
        if edits.is_empty() {
            return;
        }
        match crate::diagnostics::engine::check_edits(
            &edits,
            &mut self.lsp_baselines,
            &mut self.lsp_documents,
            &self.lsp_pool,
            &self.cx.root,
        )
        .await
        {
            Ok(diffs) => {
                if let Some(rider) = crate::diagnostics::render::build_rider(&diffs) {
                    if !content.is_empty() {
                        content.push_str("\n\n");
                    }
                    content.push_str(&rider);
                }
            }
            Err(err) => {
                tracing::warn!("window-0 diagnostics failed: {err:#}");
            }
        }
    }

    fn persist(&self) -> Result<()> {
        // Flush loop-level cells back into `side`, preserving any slots this
        // build doesn't own.
        let mut side = match self.prior_side.clone() {
            Value::Object(m) => Value::Object(m),
            _ => json!({}),
        };
        side["todos"] = self
            .todos
            .lock()
            .map(|t| t.to_side())
            .unwrap_or(Value::Null);
        side["nudges"] = self.hooks.to_side();
        side["narf_kv"] = self.kv.to_side();
        side["lsp_baselines"] = self.lsp_baselines.to_side();
        self.store.save(&SaveState {
            transport: self.tx.name(),
            model: &self.base_opts.model,
            snapshot: self.tx.snapshot(),
            side,
        })
    }
}

fn stop_reason_label(stop: Option<&StopReason>) -> Value {
    match stop {
        Some(StopReason::ToolCalls) => json!("tool_calls"),
        Some(StopReason::Done) => json!("done"),
        Some(StopReason::Length) => json!("length"),
        Some(StopReason::Other(other)) => json!({"other": other}),
        None => Value::Null,
    }
}

fn promise_summary(list: &Value) -> Value {
    let Some(entries) = list.as_array() else {
        return json!({
            "count": 0,
            "running_count": 0,
            "running_ids": [],
            "terminal_count": 0,
        });
    };
    let running_ids: Vec<Value> = entries
        .iter()
        .filter(|entry| entry["running"].as_bool() == Some(true))
        .filter_map(|entry| entry["promise_id"].as_str().map(|id| json!(id)))
        .collect();
    json!({
        "count": entries.len(),
        "running_count": running_ids.len(),
        "running_ids": running_ids,
        "terminal_count": entries.len().saturating_sub(running_ids.len()),
    })
}

fn tool_result_trace(call: &transport::ToolCall, result: &transport::ToolResult) -> Value {
    let parsed = serde_json::from_str::<Value>(&result.content).ok();
    let mut trace = json!({
        "id": call.id,
        "name": call.name,
        "is_error": result.is_error,
    });

    if let Some(body) = parsed {
        if let Some(running) = body["running"].as_bool() {
            trace["running"] = json!(running);
        }
        if let Some(session_id) = body["session_id"].as_str() {
            trace["session_id"] = json!(session_id);
        }
        if let Some(promise_id) = body["promise_id"].as_str() {
            trace["promise_id"] = json!(promise_id);
        }
        if let Some(timed_out) = body["timed_out"].as_bool() {
            trace["timed_out"] = json!(timed_out);
        }
        if body.get("exit_code").is_some() {
            trace["exit_code"] = body["exit_code"].clone();
        }
        if let Some(state) = body["state"].as_str() {
            trace["state"] = json!(state);
        }
    }

    trace
}

// ---------------------------------------------------------------------------
// Bidirectional stdin input
// ---------------------------------------------------------------------------

/// A parsed stdin message in bidirectional mode.
enum Input {
    /// A user turn (text extracted from the SDK user-message shape).
    User(String),
    /// A control request. `subtype` is read from the top level or from a nested
    /// `request` object (both Claude Agent SDK shapes are accepted).
    Control {
        subtype: String,
        req_id: Option<String>,
        raw: Value,
    },
}

/// Spawn a task that reads stdin NDJSON and forwards parsed [`Input`]s. The
/// channel closes (sender dropped) on EOF, which the session loop treats as
/// shutdown. `replay` re-emits each user message as a `user` event.
fn spawn_stdin_reader(replay: bool, emitter: Emitter) -> mpsc::UnboundedReceiver<Input> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                tracing::warn!("stdin: skipped a non-JSON line");
                continue;
            };
            match v["type"].as_str() {
                Some("user") => {
                    if replay {
                        emitter.replay_user(&v["message"]);
                    }
                    if let Some(text) = extract_user_text(&v)
                        && tx.send(Input::User(text)).is_err()
                    {
                        break;
                    }
                }
                Some("control_request") => {
                    let subtype = v["subtype"]
                        .as_str()
                        .or_else(|| v["request"]["subtype"].as_str())
                        .unwrap_or_default()
                        .to_string();
                    let req_id = v["request_id"].as_str().map(str::to_string);
                    if tx
                        .send(Input::Control {
                            subtype,
                            req_id,
                            raw: v,
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                // control_cancel_request, keep_alive, unknown → ignore.
                _ => {}
            }
        }
    });
    rx
}

/// Extract the user-turn text from an SDK `user` message: `message.content` is
/// either a string or an array of `text`/`input_text` blocks.
fn extract_user_text(v: &Value) -> Option<String> {
    let content = &v["message"]["content"];
    if let Some(s) = content.as_str() {
        return (!s.is_empty()).then(|| s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut s = String::new();
        for b in arr {
            if matches!(b["type"].as_str(), Some("text") | Some("input_text"))
                && let Some(t) = b["text"].as_str()
            {
                s.push_str(t);
            }
        }
        return (!s.is_empty()).then_some(s);
    }
    None
}

/// Rough token estimate (~4 chars/token) for the proactive compaction trigger.
/// Deliberately coarse: it only needs to flag an appended item large enough to
/// push the next request over the window, and the threshold leaves headroom.
fn est_tokens(s: &str) -> u64 {
    (s.chars().count() / 4) as u64
}

/// Estimated tokens for a batch of tool results about to be appended.
fn est_tool_results(results: &[transport::ToolResult]) -> u64 {
    results
        .iter()
        .map(|r| est_tokens(&r.content))
        .fold(0u64, u64::saturating_add)
}

/// Compose the effective system prompt as a cache-stable prefix plus a volatile
/// tail. See the transport `SystemPrompt` docs.
fn compose_system(base: Option<&str>, reg: &Registry) -> SystemPrompt {
    let mut stable = base.unwrap_or("").to_string();
    let shell_promises_only = reg.contains("shell_run") && !reg.contains("shell_poll");

    let pinned = reg.pinned();
    if !pinned.is_empty() {
        if !stable.is_empty() {
            stable.push_str("\n\n");
        }
        stable.push_str(
            "## Always-available tools\n\
             These are loaded and ready — prefer them for their purpose; do not search for them. \
             `tool_search` loads anything else on demand.\n",
        );
        for (name, desc) in pinned {
            stable.push_str(&format!("- `{name}` — {desc}\n"));
        }
        if shell_promises_only {
            stable.push_str(
                "\nShell promises: `shell_run` starts commands as harness-local Promises in fleet mode; \
                 the blocking/yield-poll path and `shell_poll` sessions are unavailable. Use \
                 `promise_wait`, `promise_status`, `promise_when_all`, `promise_when_any`, \
                 or `promise_cancel` with the returned `promise_id`. Promise completion \
                 automatically injects a hidden HARNESS_EVENT turn at a safe boundary unless a \
                 terminal wait already returned the result.\n",
            );
        } else if reg.contains("shell_poll") {
            stable.push_str(
                "\nShell sessions: if `shell_run` or `shell_poll` returns `running=true`, the command \
                 is still active. Do not treat the command as complete or stop the turn there; call \
                 `shell_poll` with the returned `session_id` until `running=false`, or `shell_kill` \
                 if you are abandoning it. For work you want to start and then continue past, call \
                 `shell_run` with `mode=\"promise\"`, then use `promise_wait`, `promise_status`, \
                 `promise_when_all`, or `promise_when_any`; promise completion automatically injects \
                 a hidden HARNESS_EVENT turn at a safe boundary unless a terminal wait already \
                 returned the result.\n",
            );
        }
    }

    let mut volatile = String::new();
    let manifest = reg.manifest();
    if !manifest.is_empty() {
        volatile.push_str(&format!(
            "## Additional tools ({} available, not yet loaded)\n\
             Call `tool_search(\"<keywords>\")` (or `tool_search(\"select:name1,name2\")`) to load \
             any of these before using them:\n",
            manifest.len()
        ));
        for (name, desc) in manifest {
            volatile.push_str(&format!("- {name}: {desc}\n"));
        }
    }

    SystemPrompt {
        stable: (!stable.is_empty()).then_some(stable),
        volatile: (!volatile.is_empty()).then_some(volatile),
    }
}

fn resolve_prompt(cli: &Cli) -> Result<String> {
    if let Some(p) = &cli.prompt {
        return Ok(p.clone());
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read prompt from stdin")?;
    if buf.trim().is_empty() {
        anyhow::bail!("no prompt: neither -p nor stdin provided one");
    }
    Ok(buf)
}

fn env_u32(key: &str) -> Option<u32> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}
fn env_u64(key: &str) -> Option<u64> {
    std::env::var(key).ok().and_then(|v| v.parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fleet_builtin_tools_replace_shell_sessions_with_promises() {
        let names: HashSet<_> = builtin_tools_for_mode(true)
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.contains("shell_run"));
        assert!(!names.contains("shell_poll"));
        assert!(!names.contains("shell_kill"));
        assert!(!names.contains("shell_list"));
        assert!(names.contains("promise_wait"));
        assert!(names.contains("promise_status"));
    }

    #[test]
    fn non_fleet_builtin_tools_keep_shell_sessions() {
        let names: HashSet<_> = builtin_tools_for_mode(false)
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.contains("shell_run"));
        assert!(names.contains("shell_poll"));
        assert!(names.contains("shell_kill"));
        assert!(names.contains("shell_list"));
    }

    #[tokio::test]
    async fn fleet_shell_run_always_returns_a_promise() {
        let c = ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(Mutex::new(bro_tools::PromiseStore::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
        };

        let v = match FleetShellRun
            .call(json!({"command": "echo fleet", "yield_time_ms": 0}), &c)
            .await
        {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(v["running"], true, "{v}");
        assert!(v["promise_id"].as_str().is_some(), "{v}");
        assert!(
            c.shell_sessions.lock().unwrap().is_empty(),
            "fleet shell_run must not create shell_poll sessions"
        );

        let waited = match bro_tools::promise::PromiseWait
            .call(
                json!({"promise_id": v["promise_id"], "timeout_ms": 3000}),
                &c,
            )
            .await
        {
            ToolResult::Json(v) => v,
            other => panic!("expected json, got {other:?}"),
        };
        assert_eq!(waited["state"], "completed", "{waited}");
        assert_eq!(waited["result"]["exit_code"], 0, "{waited}");
        assert_eq!(waited["result"]["stdout"], "fleet\n", "{waited}");
    }

    #[test]
    fn extract_user_text_string_and_blocks() {
        let s = json!({"type": "user", "message": {"role": "user", "content": "hello"}});
        assert_eq!(extract_user_text(&s).as_deref(), Some("hello"));

        let blocks = json!({"type": "user", "message": {"role": "user", "content": [
            {"type": "text", "text": "a"},
            {"type": "input_text", "text": "b"},
            {"type": "image", "source": {}},
        ]}});
        assert_eq!(extract_user_text(&blocks).as_deref(), Some("ab"));

        let empty = json!({"type": "user", "message": {"role": "user", "content": ""}});
        assert_eq!(extract_user_text(&empty), None);
    }

    // --- bidirectional session_loop integration (mock transport) ------------

    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Notify;

    enum MockTurn {
        /// Return text immediately (Done, no tool calls).
        Text(String),
        /// Wait on the shared gate, then request a tool call.
        ToolCallAfterGate,
        /// Await a gate that tests never release — to be cancelled by interrupt.
        Block,
    }

    #[derive(Clone, Default)]
    struct MockShared {
        pushed_users: Arc<Mutex<Vec<String>>>,
        started: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
        compact_calls: Arc<AtomicUsize>,
        model_gate: Arc<Notify>,
        tool_started: Arc<AtomicUsize>,
        tool_gate: Arc<Notify>,
    }

    struct MockTransport {
        shared: MockShared,
        scripts: Arc<Mutex<VecDeque<MockTurn>>>,
    }

    #[async_trait]
    impl Transport for MockTransport {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn push_user_text(&mut self, text: &str) {
            self.shared
                .pushed_users
                .lock()
                .unwrap()
                .push(text.to_string());
        }
        fn push_tool_results(&mut self, _results: Vec<transport::ToolResult>) {}
        async fn run_turn(
            &mut self,
            _tools: &[transport::ToolSpec],
            _opts: &TurnOpts,
            _sink: &dyn transport::TurnSink,
        ) -> Result<transport::TurnOutput> {
            self.shared.started.fetch_add(1, Ordering::SeqCst);
            let script = self
                .scripts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(MockTurn::Text("ok".into()));
            match script {
                MockTurn::Block => {
                    self.shared.model_gate.notified().await;
                    unreachable!("gate is never released in tests");
                }
                MockTurn::ToolCallAfterGate => {
                    self.shared.model_gate.notified().await;
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: String::new(),
                        thinking: String::new(),
                        tool_calls: vec![transport::ToolCall {
                            id: "tool-1".into(),
                            name: "slow_tool".into(),
                            args: json!({}),
                        }],
                        stop: StopReason::ToolCalls,
                        usage: Usage::default(),
                    })
                }
                MockTurn::Text(t) => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: t,
                        thinking: String::new(),
                        tool_calls: vec![],
                        stop: StopReason::Done,
                        usage: Usage::default(),
                    })
                }
            }
        }
        fn snapshot(&self) -> Value {
            json!([])
        }
        fn restore(&mut self, _snapshot: Value) {}
        async fn compact(
            &mut self,
            _params: transport::CompactionParams,
            _instruction: &str,
            _tools: &[transport::ToolSpec],
            _opts: &TurnOpts,
        ) -> Result<Option<String>> {
            self.shared.compact_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some("summary".into()))
        }
    }

    struct SlowTool {
        shared: MockShared,
    }

    #[async_trait]
    impl bro_tools::Tool for SlowTool {
        fn name(&self) -> &str {
            "slow_tool"
        }

        fn description(&self) -> &str {
            "test-only slow tool"
        }

        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }

        async fn call(&self, _input: Value, _cx: &ToolCx) -> bro_tools::ToolResult {
            self.shared.tool_started.fetch_add(1, Ordering::SeqCst);
            self.shared.tool_gate.notified().await;
            bro_tools::ToolResult::Text("slow done".into())
        }
    }

    fn mk_session(scripts: Vec<MockTurn>) -> (Session, MockShared) {
        let shared = MockShared::default();
        let mock = MockTransport {
            shared: shared.clone(),
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
        };
        let todos = Arc::new(Mutex::new(bro_tools::TodoList::default()));
        let kv = Arc::new(crate::capabilities::KvStore::default());
        let cx = ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            promises: Arc::new(Mutex::new(bro_tools::PromiseStore::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = format!("bh-test-{}-{}", std::process::id(), nanos);
        let session = Session {
            tx: Box::new(mock),
            reg: Registry::new(
                vec![Arc::new(SlowTool {
                    shared: shared.clone(),
                })],
                vec![],
                &PinPolicy::from_env(),
                &mcp::ToolFilter::default(),
            ),
            cx,
            hooks: HookEngine::from_env(NudgeLedger::from_side(&Value::Null)),
            emitter: Emitter::new("test".into()),
            base_opts: TurnOpts {
                model: "m".into(),
                max_tokens: 8,
                system: SystemPrompt::default(),
                effort: None,
                web_search: false,
                service_tier: None,
            },
            system: None,
            max_turns: 50,
            compaction: crate::compaction::CompactionPolicy::from_env(),
            compact_threshold: None,
            tool_result_cap: 0,
            dump_dir: std::env::temp_dir(),
            store: SessionStore::open(Some(&id), None).unwrap(),
            prior_side: Value::Null,
            todos,
            kv,
            lsp_baselines: LspBaselines::default(),
            lsp_pool: bro_lsp::SessionPool::new(bro_lsp::LspConfig::default()),
            lsp_documents: BTreeMap::new(),
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
            pending_input_estimate: 0,
            tail_nudge: None,
        };
        (session, shared)
    }

    #[tokio::test]
    async fn session_loop_processes_turns_in_order() {
        let (mut session, shared) =
            mk_session(vec![MockTurn::Text("1".into()), MockTurn::Text("2".into())]);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("alpha".into())).unwrap();
        tx.send(Input::User("beta".into())).unwrap();
        drop(tx); // EOF after both
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();
        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(users, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(shared.completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn max_turns_is_per_user_turn_in_persistent_session() {
        let (mut session, shared) =
            mk_session(vec![MockTurn::Text("1".into()), MockTurn::Text("2".into())]);
        session.max_turns = 1;
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("alpha".into())).unwrap();
        tx.send(Input::User("beta".into())).unwrap();
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();
        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(users, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(shared.completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn interrupt_cancels_in_flight_turn() {
        let (mut session, shared) = mk_session(vec![MockTurn::Block]);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("go".into())).unwrap();
        tx.send(Input::Control {
            subtype: "interrupt".into(),
            req_id: Some("r1".into()),
            raw: json!({}),
        })
        .unwrap();
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        // Without cancellation the Block turn hangs forever; the timeout proves
        // the interrupt unwound it and the loop drained to EOF.
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            session_loop(&mut session, rx, &ctrl, VecDeque::new()),
        )
        .await
        .expect("session_loop must not hang on an interrupted turn")
        .unwrap();
        assert_eq!(shared.started.load(Ordering::SeqCst), 1, "turn started");
        assert_eq!(
            shared.completed.load(Ordering::SeqCst),
            0,
            "the blocked turn was cancelled, never completed"
        );
    }

    #[tokio::test]
    async fn stdin_steer_during_tool_turn_injects_before_next_model_call() {
        let (mut session, shared) = mk_session(vec![
            MockTurn::ToolCallAfterGate,
            MockTurn::Text("done".into()),
        ]);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("alpha".into())).unwrap();
        let ctrl = Emitter::new("ctrl".into());
        let run =
            tokio::spawn(
                async move { session_loop(&mut session, rx, &ctrl, VecDeque::new()).await },
            );

        loop {
            if shared.started.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        shared.model_gate.notify_waiters();
        loop {
            if shared.tool_started.load(Ordering::SeqCst) >= 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
        tx.send(Input::User("beta".into())).unwrap();
        tokio::task::yield_now().await;
        shared.tool_gate.notify_waiters();
        loop {
            if shared.completed.load(Ordering::SeqCst) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        drop(tx);

        tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .expect("session_loop must finish")
            .expect("session task must not panic")
            .unwrap();
        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(users, vec!["alpha".to_string(), "beta".to_string()]);
        assert_eq!(shared.completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn slash_compact_runs_compaction_not_a_turn() {
        let (mut session, shared) = mk_session(vec![]);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("/compact".into())).unwrap();
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();
        assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            shared.started.load(Ordering::SeqCst),
            0,
            "/compact is a slash command, not a model turn"
        );
    }

    #[tokio::test]
    async fn proactive_trigger_compacts_on_appended_estimate() {
        // last_prompt_tokens stays 0 (the mock reports no usage), but the
        // estimated tokens of the appended user message cross a tiny threshold —
        // so the proactive trigger compacts *before* the would-be-over-window
        // call, rather than only reacting after observing usage.
        let (mut session, shared) = mk_session(vec![MockTurn::Text("done".into())]);
        session.compact_threshold = Some(1);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::User("x".repeat(40))).unwrap(); // est_tokens = 10 > 1
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();
        assert_eq!(
            shared.compact_calls.load(Ordering::SeqCst),
            1,
            "appended-tail estimate should trigger compaction before the call"
        );
    }

    #[test]
    fn est_tokens_counts_chars_over_four() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abcd"), 1);
        assert_eq!(est_tokens(&"x".repeat(400)), 100);
    }

    #[tokio::test]
    async fn idle_set_model_control_mutates_model() {
        let (mut session, _shared) = mk_session(vec![]);
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(Input::Control {
            subtype: "set_model".into(),
            req_id: Some("r".into()),
            raw: json!({"type": "control_request", "subtype": "set_model", "model": "new-model"}),
        })
        .unwrap();
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();
        assert_eq!(session.base_opts.model, "new-model");
    }

    #[tokio::test]
    async fn settled_promise_completion_injects_hidden_turn() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("completion handled".into())]);
        let (pid, _cancel_rx) = session.cx.promises.lock().unwrap().start(
            "shell_run",
            json!({"command": "echo done"}),
            None,
        );
        session
            .cx
            .promises
            .lock()
            .unwrap()
            .settle_completed(&pid, json!({"exit_code": 0, "stdout": "done\n"}));

        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, VecDeque::new())
            .await
            .unwrap();

        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(users.len(), 1, "{users:?}");
        assert!(users[0].contains("[HARNESS_EVENT promise_completed]"));
        assert!(users[0].contains(&pid));
        assert!(users[0].contains("call promise_status"));
    }

    #[test]
    fn turn_end_diagnostics_flags_running_async_work() {
        let (session, _shared) = mk_session(vec![]);
        let (pid, _cancel_rx) = session.cx.promises.lock().unwrap().start(
            "shell_run",
            json!({"command": "cargo test"}),
            None,
        );

        let trace = tool_result_trace(
            &transport::ToolCall {
                id: "tc-1".into(),
                name: "shell_run".into(),
                args: json!({}),
            },
            &transport::ToolResult {
                id: "tc-1".into(),
                content: json!({
                    "promise_id": pid,
                    "state": "running",
                    "running": true,
                })
                .to_string(),
                is_error: false,
            },
        );

        // Non-empty final text proves the async path is flagged independently
        // of the empty-output heuristic.
        let diag = session.turn_end_diagnostics(
            "model_stop",
            Some(&StopReason::Done),
            0,
            1,
            &[trace],
            "all set, kicking off the build",
        );

        assert_eq!(diag["break_reason"], "model_stop");
        assert_eq!(diag["last_model_stop"], "done");
        assert_eq!(diag["promises"]["running_count"], 1);
        assert_eq!(diag["last_tool_results"][0]["running"], true);
        assert_eq!(diag["produced_text"], true);
        assert_eq!(diag["empty_output_stop"], false);
        assert_eq!(diag["suspicious"], true);
        assert!(
            diag["suspicion_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "running_promises"),
            "{diag}"
        );
    }

    #[test]
    fn turn_end_diagnostics_flags_empty_output_model_stop() {
        // The classic spurious stop: model ends the turn (end_turn → Done) with
        // no text and no tool calls, and there is no outstanding async work.
        let (session, _shared) = mk_session(vec![]);

        let diag =
            session.turn_end_diagnostics("model_stop", Some(&StopReason::Done), 0, 1, &[], "");

        assert_eq!(diag["produced_text"], false);
        assert_eq!(diag["final_text_len"], 0);
        assert_eq!(diag["empty_output_stop"], true);
        assert_eq!(diag["suspicious"], true);
        assert_eq!(diag["suspicion_reasons"][0], "empty_output_stop");
    }

    #[test]
    fn turn_end_diagnostics_normal_text_stop_is_not_suspicious() {
        // A model that ends the turn with substantive text and no outstanding
        // work is a normal answer, not a spurious stop.
        let (session, _shared) = mk_session(vec![]);

        let diag = session.turn_end_diagnostics(
            "model_stop",
            Some(&StopReason::Done),
            0,
            2,
            &[],
            "Here is the summary of what I changed.",
        );

        assert_eq!(diag["produced_text"], true);
        assert_eq!(diag["empty_output_stop"], false);
        assert_eq!(diag["suspicious"], false);
        assert!(diag["suspicion_reasons"].as_array().unwrap().is_empty());
    }

    #[test]
    fn turn_end_diagnostics_empty_max_turns_is_not_empty_output_stop() {
        // max_turns / cancel / interrupt are harness-driven ends, not the model
        // returning nothing — they must not be laundered into empty_output_stop.
        let (session, _shared) = mk_session(vec![]);

        let diag =
            session.turn_end_diagnostics("max_turns", Some(&StopReason::ToolCalls), 1, 50, &[], "");

        assert_eq!(diag["empty_output_stop"], false);
        assert_eq!(diag["suspicious"], false);
    }

    // ---- window-0 diagnostics: full seam (drain edits -> engine -> render -> rider) ----

    fn ra_runs(p: &std::path::Path) -> bool {
        std::process::Command::new(p)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    fn ra_bin() -> Option<std::path::PathBuf> {
        for key in [
            "BRO_LSP_RUST_ANALYZER_BIN",
            "BRO_RUST_ANALYZER_BIN",
            "BLACKBOX_RUST_ANALYZER_BIN",
        ] {
            if let Ok(v) = std::env::var(key) {
                let p = std::path::PathBuf::from(v.trim());
                if !p.as_os_str().is_empty() && ra_runs(&p) {
                    return Some(p);
                }
            }
        }
        if ra_runs(std::path::Path::new("rust-analyzer")) {
            return Some(std::path::PathBuf::from("rust-analyzer"));
        }
        let cargo_bin =
            std::path::PathBuf::from(std::env::var_os("HOME")?).join(".cargo/bin/rust-analyzer");
        ra_runs(&cargo_bin).then_some(cargo_bin)
    }

    struct TmpProject(std::path::PathBuf);
    impl Drop for TmpProject {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// End-to-end proof of the window-0 path that neither isolated unit test
    /// covers: a real edit on disk, driven through the actual seam method, must
    /// produce a diagnostics rider mentioning the new error. RA-gated.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn window0_rider_surfaces_new_error_end_to_end() -> Result<()> {
        let Some(ra) = ra_bin() else {
            eprintln!("skipping window-0 e2e test: rust-analyzer not found");
            return Ok(());
        };

        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("w0-e2e-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("src"))?;
        let _guard = TmpProject(root.clone());
        let root = root.canonicalize()?;
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"w0_e2e_fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
        )?;
        let file = root.join("src/lib.rs");
        let clean = "pub fn value() -> u32 {\n    let x: u32 = 1;\n    x\n}\n";
        let broken = "pub fn value() -> u32 {\n    let x: u32 = \"s\";\n    x\n}\n";
        // the edit has already landed on disk, as file_write/file_edit leaves it
        std::fs::write(&file, broken)?;

        let (mut session, _shared) = mk_session(vec![]);
        session.cx.root = root.clone();
        session.lsp_pool = bro_lsp::SessionPool::new(bro_lsp::LspConfig {
            ready_timeout: std::time::Duration::from_secs(5),
            rust_analyzer_bin: Some(ra),
            ..Default::default()
        });
        // record the mutation in the per-dispatch sink, exactly as the file tools do
        session
            .cx
            .edits
            .lock()
            .unwrap()
            .push(bro_tools::edits::EditEvent::from_bytes(
                file.clone(),
                clean.as_bytes(),
                broken.as_bytes(),
            ));

        let mut content = "{\"ok\":true}".to_string();
        session.append_window0_diagnostics(&mut content).await;

        assert!(
            content.contains("diagnostics:"),
            "expected a window-0 diagnostics rider, got: {content}"
        );
        assert!(
            content.contains("src/lib.rs"),
            "rider should name the edited file, got: {content}"
        );
        assert!(
            content.contains("error"),
            "rider should report the new error, got: {content}"
        );

        session.lsp_pool.shutdown_all().await;
        Ok(())
    }

    /// No edits recorded in the dispatch -> the seam appends nothing (no RA).
    #[tokio::test]
    async fn window0_diagnostics_noop_without_edits() {
        let (mut session, _shared) = mk_session(vec![]);
        let mut content = "{\"ok\":true}".to_string();
        session.append_window0_diagnostics(&mut content).await;
        assert_eq!(content, "{\"ok\":true}", "no edits -> no rider appended");
    }
}

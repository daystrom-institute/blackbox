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

use crate::capabilities::HarnessSessionServices;
use crate::cli::Cli;
use crate::emit::{Emitter, EventCallback};
use crate::event_log::EventLog;
use crate::hooks::{Delivery, HookEngine, NudgeLedger};
use crate::lsp_baselines::LspBaselines;
use crate::mcp;
use crate::registry::{PinPolicy, Registry};
use crate::session::SessionStore;
use crate::transport::{self, StopReason, SystemPrompt, Transport, TransportKind, TurnOpts, Usage};
use anyhow::{Context, Result};
use bro_tools::{SafetyPolicy, Tool, ToolCx, builtin_tools};
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
/// Runaway backstop on loop iterations *per user turn*, set far above any real
/// task — the daemon's supervision plus operator observation are the actual
/// guards, so this only exists to stop a truly stuck loop. It must never
/// guillotine a legitimate session: each iteration is one model round-trip
/// (~one tool call for sequential-tool models), so a normal grounding +
/// implement + build-poll sequence can easily run into the hundreds. Override
/// with `BRO_HARNESS_MAX_TURNS`.
const DEFAULT_MAX_TURNS: u64 = 1000;

/// Marker injected as a tool_result when a tool dispatch is interrupted, so the
/// transport buffer stays valid (every tool_use gets a matching result).
const INTERRUPTED_TOOL_RESULT: &str = "[Request interrupted by user]";

/// Name of the synthetic terminal tool for structured output.
const FINAL_RESULT_TOOL: &str = "final_result";

/// System-prompt instruction appended when structured output is active.
const STRUCTURED_OUTPUT_INSTRUCTION: &str = "\
When you have your final answer, call the `final_result` tool with arguments \
that conform to its input schema. That call ends the session — do not make any \
further tool calls after it.";

// ---------------------------------------------------------------------------
// FinalResultTool — synthetic terminal tool for structured output
// ---------------------------------------------------------------------------

/// A synthetic tool registered when `--output-schema` is provided. Its
/// `input_schema` is the user-supplied JSON schema. When the model calls it,
/// the agent loop captures the arguments as the structured result and
/// terminates the session cleanly.
struct FinalResultTool {
    schema: Value,
}

impl FinalResultTool {
    fn new(schema: Value) -> Self {
        Self { schema }
    }
}

#[async_trait::async_trait]
impl Tool for FinalResultTool {
    fn name(&self) -> &str {
        FINAL_RESULT_TOOL
    }

    fn description(&self) -> &str {
        "Submit your final structured result. Call this tool when you have completed \
         the task and have a final answer conforming to the output schema. This ends \
         the session."
    }

    fn input_schema(&self) -> Value {
        self.schema.clone()
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> bro_tools::ToolResult {
        // The agent loop intercepts `final_result` before normal dispatch,
        // so this body is a fallback. Return the captured args as JSON.
        bro_tools::ToolResult::Json(input)
    }
}

/// Entry point. Branches one-shot vs. bidirectional on `--input-format`.
pub async fn run(cli: Cli) -> Result<()> {
    run_with_emitter(cli, None, None, HarnessSessionServices::standalone()).await
}

pub async fn run_with_event_callback(cli: Cli, callback: EventCallback) -> Result<()> {
    run_with_emitter(
        cli,
        Some(callback),
        None,
        HarnessSessionServices::standalone(),
    )
    .await
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
    run_with_event_callback_and_input_mcp(
        cli,
        input_rx,
        callback,
        None,
        None,
        None,
        HarnessSessionServices::standalone(),
    )
    .await
}

pub async fn run_with_event_callback_and_input_mcp(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: EventCallback,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, String>>,
    shell_env: Option<BTreeMap<String, String>>,
    services: HarnessSessionServices,
) -> Result<()> {
    run_controlled_session(
        cli,
        input_rx,
        Some(callback),
        mcp_config,
        additional_context,
        shell_env,
        services,
    )
    .await
}

async fn run_with_emitter(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
    services: HarnessSessionServices,
) -> Result<()> {
    if cli.input_format.as_deref() == Some("stream-json") {
        return run_session(cli, callback, mcp_config, None, services).await;
    }

    // One-shot: a single prompt, one user turn, then persist and exit.
    let prompt = resolve_prompt(&cli)?;
    let mut session = Session::build(&cli, callback, mcp_config, None, None, services).await?;
    session.emitter.system_init();
    // A cancel channel that never fires — one-shot turns are not interruptible.
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    session
        .user_turn(&prompt, cancel_rx, Arc::new(StdMutex::new(VecDeque::new())))
        .await?;
    let body = session.persist_body()?;
    let path = session.store_path().to_path_buf();
    tokio::task::spawn_blocking(move || crate::session::write_atomic(&path, &body))
        .await
        .context("persist task panicked")?
        .context("write session")?;
    Ok(())
}

fn make_emitter(
    session_id: String,
    callback: Option<EventCallback>,
    event_log: Option<Arc<EventLog>>,
) -> Emitter {
    let emitter = match callback {
        Some(callback) => Emitter::with_callback(session_id, callback),
        None => Emitter::new(session_id),
    };
    match event_log {
        Some(log) => emitter.with_event_log(log),
        None => emitter,
    }
}

/// Bidirectional persistent session driven over stdin NDJSON.
async fn run_session(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, String>>,
    services: HarnessSessionServices,
) -> Result<()> {
    let replay = cli.replay_user_messages;
    let mut session = Session::build(
        &cli,
        callback.clone(),
        mcp_config,
        additional_context,
        None,
        services,
    )
    .await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();

    // The stdin reader runs as its own task so control messages (interrupt)
    // arrive while a turn is in flight. It owns a clone of the emitter purely to
    // honour `--replay-user-messages`.
    let input_rx = spawn_stdin_reader(
        replay,
        make_emitter(sid.clone(), callback.clone(), Some(session.event_log())),
    );
    // A separate emitter for control responses emitted *during* a turn, when the
    // session's own emitter is borrowed by the running turn.
    let ctrl_emitter = make_emitter(sid, callback, Some(session.event_log()));

    // Steers that arrived mid-turn wait here for the next turn boundary.
    let mut pending: VecDeque<String> = VecDeque::new();
    // An initial `-p` prompt (if any) is the first user turn.
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }

    session_loop(&mut session, input_rx, &ctrl_emitter, pending).await?;
    let body = session.persist_body()?;
    let path = session.store_path().to_path_buf();
    tokio::task::spawn_blocking(move || crate::session::write_atomic(&path, &body))
        .await
        .context("persist task panicked")?
        .context("write session")?;
    Ok(())
}

async fn run_controlled_session(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, String>>,
    shell_env: Option<BTreeMap<String, String>>,
    services: HarnessSessionServices,
) -> Result<()> {
    let mut session = Session::build(
        &cli,
        callback.clone(),
        mcp_config,
        additional_context,
        shell_env,
        services,
    )
    .await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();
    let ctrl_emitter = make_emitter(sid, callback, Some(session.event_log()));

    let mut pending: VecDeque<String> = VecDeque::new();
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }
    let input_rx = map_session_input(input_rx);

    session_loop_until_idle(&mut session, input_rx, &ctrl_emitter, pending).await?;
    let body = session.persist_body()?;
    let path = session.store_path().to_path_buf();
    tokio::task::spawn_blocking(move || crate::session::write_atomic(&path, &body))
        .await
        .context("persist task panicked")?
        .context("write session")?;
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
            None => match input_rx.recv().await {
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
        match session.persist_body() {
            Ok(body) => {
                let path = session.store_path().to_path_buf();
                // Move the write off the async runtime.
                let write_res =
                    tokio::task::spawn_blocking(move || crate::session::write_atomic(&path, &body))
                        .await;
                if let Err(e) = match write_res {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.context("write session")),
                    Err(je) => Err(anyhow::anyhow!("persist task panicked: {je}")),
                } {
                    tracing::warn!("failed to persist session after turn: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!("failed to serialize session after turn: {e:#}");
            }
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
        match session.persist_body() {
            Ok(body) => {
                let path = session.store_path().to_path_buf();
                let write_res =
                    tokio::task::spawn_blocking(move || crate::session::write_atomic(&path, &body))
                        .await;
                if let Err(e) = match write_res {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.context("write session")),
                    Err(je) => Err(anyhow::anyhow!("persist task panicked: {je}")),
                } {
                    tracing::warn!("failed to persist session after controlled turn: {e:#}");
                }
            }
            Err(e) => {
                tracing::warn!("failed to serialize session after controlled turn: {e:#}");
            }
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
        session
            .emitter
            .result_error(&format!("{e:#}"), session.turns);
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
    /// Explicit services retained for the full session lifetime. Model-facing
    /// tools hold clones of the clients they use today; unprojected clients,
    /// such as execution, remain available for later worker protocol slices.
    _services: HarnessSessionServices,
    /// Resolved code-mode for this session. Session-intrinsic (like `model`):
    /// persisted in the session file and restored on resume so the surface
    /// shape stays consistent with any `exec` cells already in the transcript.
    code_mode: crate::code_mode::CodeMode,
    cx: ToolCx,
    reference_context_item: Option<crate::context::TurnContextItem>,
    hooks: HookEngine,
    scoped_project_docs: crate::project_doc::ScopedProjectDocs,
    emitter: Emitter,
    base_opts: TurnOpts,
    /// Explicit caller-supplied system text. Discovered AGENTS/UserInstructions
    /// lives in `user_instructions`, not in the system slot.
    explicit_system: Option<String>,
    user_instructions: Option<crate::context::UserInstructions>,
    /// Per-transport composition strategy: where persona, directives, memory,
    /// scope, and pins land for this session's transport
    /// (design/bro-harness/dispatch-prompt-slots.md §5).
    strategy: crate::context::dispatch::CompositionStrategy,
    /// Typed dispatch-context state (`--dispatch-context`): the current
    /// in-memory context plus the last-emitted user-lane baselines. Persisted
    /// in the `dispatch_context` / `dispatch_emitted` side cells (scope is
    /// NEVER persisted/restored — per-dispatch correlation data).
    dispatch: crate::context::dispatch::DispatchState,
    max_turns: u64,
    compaction: crate::compaction::CompactionPolicy,
    compact_threshold: Option<u64>,
    /// Tool-result spill threshold in bytes (0 ⇒ disabled) and the dump dir.
    tool_result_cap: usize,
    dump_dir: std::path::PathBuf,
    store: SessionStore,
    /// Sidecar append-only timestamped event log (`event_log.rs`). The
    /// emitters tee every protocol event into it; the loop additionally logs
    /// user turns and compaction milestones. Best-effort — never fails a turn.
    event_log: Arc<EventLog>,
    prior_side: Value,
    todos: Arc<std::sync::Mutex<bro_tools::TodoList>>,
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
    /// When set, a synthetic `final_result` tool was registered whose
    /// `input_schema` is this JSON schema. The agent is instructed to call
    /// `final_result` with its final answer; the turn loop captures the args
    /// and terminates cleanly.
    output_schema: Option<Value>,
}

/// Whether this session may request the provider's server-side `web_search`
/// tool (`BRO_HARNESS_WEB_SEARCH`, absent ⇒ enabled). Resolved through the
/// per-session env (`transport::session_var`) — NOT raw process env — so an
/// in-process host's per-dispatch `env_overrides` can disable it lane-by-lane
/// (e.g. the daemon's glm lane defaults it off); the standalone binary still
/// honors the operator's shell env via the `session_var` fallback.
fn web_search_enabled() -> bool {
    transport::session_var("BRO_HARNESS_WEB_SEARCH")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

impl Session {
    // one-time session construction; cwd canonicalize happens before the loop serves turns.
    #[allow(clippy::disallowed_methods)]
    async fn build(
        cli: &Cli,
        callback: Option<EventCallback>,
        injected_mcp: Option<mcp::McpConfig>,
        additional_context: Option<BTreeMap<String, String>>,
        shell_env: Option<BTreeMap<String, String>>,
        mut services: HarnessSessionServices,
    ) -> Result<Self> {
        if let Some(fmt) = cli.output_format.as_deref()
            && fmt != "stream-json"
        {
            anyhow::bail!("unsupported --output-format {fmt}; only stream-json");
        }

        let max_tokens = env_u32("BRO_HARNESS_MAX_TOKENS").unwrap_or(DEFAULT_MAX_TOKENS);
        let max_turns = env_u64("BRO_HARNESS_MAX_TURNS").unwrap_or(DEFAULT_MAX_TURNS);
        let web_search = web_search_enabled();

        // Three-state --system-prompt:
        //   non-empty ⇒ explicit override, kept verbatim in the system slot;
        //   ""        ⇒ explicit suppress (no system prompt, no AGENTS fragment);
        //   absent    ⇒ not overridden ⇒ Codex-style AGENTS.md discovery moves
        //               to UserInstructions in the contextual user message.
        // Per-session working directory: explicit `--cwd` (the daemon's
        // dispatch cwd, passed instead of mutating the process cwd) or the
        // process cwd for the standalone binary. All file/shell tools and
        // project-doc discovery resolve against this root, so concurrent
        // in-process sessions never collide (harness-daemon-boundary.md §3).
        let root = match cli.cwd.as_deref() {
            Some(c) => std::fs::canonicalize(c).unwrap_or_else(|_| std::path::PathBuf::from(c)),
            None => std::env::current_dir().context("cwd")?,
        };

        let (explicit_system, user_instructions) = match cli.system_prompt.as_deref() {
            Some("") => (None, None),
            Some(s) => (Some(s.to_string()), None),
            None => (None, crate::context::UserInstructions::from_project(&root)),
        };

        let kind = TransportKind::from_env();
        let mut tx = transport::build_transport(kind).await?;

        let store = SessionStore::open(cli.session_id.as_deref(), cli.resume.as_deref())?;
        // Sidecar append-only event log next to the snapshot — the durable
        // timestamped record of this session (event_log.rs).
        let event_log = Arc::new(EventLog::for_session(&store.id));
        let scoped_project_docs = crate::project_doc::ScopedProjectDocs::from_startup_and_event_log(
            user_instructions
                .as_ref()
                .map(|instructions| instructions.loaded_paths.clone())
                .unwrap_or_default(),
            event_log.path(),
        );
        // Hand the transport the stable session id, so it can populate the
        // codex-style `session-id` header + `prompt_cache_key` (vs a random
        // per-request id).
        tx.set_session_id(store.id.clone());
        let restored_model = store.restored.as_ref().and_then(|r| r.model.clone());
        let restored_code_mode = store.restored.as_ref().and_then(|r| r.code_mode.clone());
        let restored_service_tier = store.restored.as_ref().and_then(|r| r.service_tier.clone());
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
        let hooks = HookEngine::from_env(NudgeLedger::from_side(
            prior_side.get("nudges").unwrap_or(&Value::Null),
        ));
        let lsp_baselines =
            LspBaselines::from_side(prior_side.get("lsp_baselines").unwrap_or(&Value::Null));
        let restored_reference_context = crate::context::TurnContextItem::from_side(
            prior_side.get("reference_context").unwrap_or(&Value::Null),
        );
        // Dispatch-context resolution (dispatch-prompt-slots.md §4): the flag
        // replaces the persisted context wholesale; empty clears; absent
        // restores persona/pins/non-`needs_scope` directives from side-state
        // with scope dropped. Strict parse — daemon-authored payloads fail
        // loudly, they do not degrade.
        let dispatch_arg =
            crate::context::dispatch::resolve_dispatch_context_arg(cli.dispatch_context.as_deref())
                .map_err(anyhow::Error::msg)
                .context("--dispatch-context")?;
        let dispatch = crate::context::dispatch::DispatchState::from_arg(dispatch_arg, &prior_side);
        let strategy = crate::context::dispatch::CompositionStrategy::for_transport(kind);
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
        let restored_snapshot = store.restored.is_some();

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
        let tool_arg_defaults =
            load_tool_arg_defaults(additional_context, cli.additional_context.as_deref())?;
        let shell_env = load_shell_env(shell_env, cli.shell_env.as_deref())?;

        let edits = Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default()));
        let cx = ToolCx {
            root: root.clone(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: edits.clone(),
            session_env: Arc::new(transport::session_env_snapshot()),
            tool_arg_defaults: Arc::new(tool_arg_defaults),
            shell_env: Arc::new(shell_env),
        };
        // Stage 1 has no rollout reconstruction yet. On resume, seed the
        // context baseline gate for legacy sessions with no persisted
        // `reference_context` so the already-persisted conversation is not
        // front-loaded with a second fresh <environment_context>.
        let reference_context_item =
            reference_context_item_for_restore(restored_snapshot, restored_reference_context, &cx);
        // The builtin `report` tool is harness-owned (it emits the cockpit's
        // status signal on the stream) and holds its own emitter handle. It is
        // registered always but only pinned in fleet (bidirectional) mode.
        let fleet = cli.input_format.as_deref() == Some("stream-json");
        let tool_filter =
            mcp::ToolFilter::from_csv(cli.deny_tools.as_deref(), cli.allow_tools.as_deref());
        let mut builtins = builtin_tools();
        // Grammar-transport rule: a tool that REQUIRES a freeform grammar (e.g.
        // apply_patch) is only meaningful where the transport honors grammars
        // (Responses). On Anthropic/Chat it would degrade to an unconstrained
        // JSON-string tool competing with file_edit, so drop it rather than
        // offer it unconstrained. (code-mode's exec/wait are added later, below,
        // so this never touches them.)
        if !kind.honors_grammar() {
            builtins.retain(|t| t.freeform_grammar().is_none());
        }
        builtins.push(Arc::new(crate::report::ReportTool::new(make_emitter(
            store.id.clone(),
            callback.clone(),
            Some(event_log.clone()),
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
        let mcp_all_for_code_mode: Vec<Arc<dyn Tool>> = mcp_in_box
            .iter()
            .chain(mcp_out_box.iter())
            .cloned()
            .collect();
        // Session-scoped capability bindings (harness-daemon-boundary.md §4):
        // derive direct trait-dispatch tools from this session's explicit
        // service set. The empty standalone default fails closed by absence.
        // Registered as builtins so the normal ToolFilter still gates them.
        builtins.extend(services.capability_tools());
        // Code-mode (exec/wait) supersedes NARF as the authorial surface. The
        // callable set mirrors the flat surface — filtered builtins + capability
        // tools + all MCP — and a ToolCapability seam over that same set
        // dispatches a cell's nested tools.* (deny-filter honored; exec/wait are
        // excluded from the projected namespace so a cell cannot relaunch the box).
        // Resolved code-mode: explicit --code-mode wins; on resume fall back to
        // the value persisted with the session (the daemon doesn't re-pass it,
        // mirroring --model); then the env default; then `optional`.
        let code_mode = cli
            .code_mode
            .clone()
            .or(restored_code_mode)
            .or_else(|| std::env::var("BRO_HARNESS_CODE_MODE").ok())
            .map(|v| crate::code_mode::CodeMode::parse_or_default(&v))
            .unwrap_or_default();

        let mut cm_callable: Vec<Arc<dyn Tool>> = builtins
            .iter()
            .filter(|t| tool_filter.permits(t.name()))
            .cloned()
            .collect();
        cm_callable.extend(mcp_all_for_code_mode);
        let mut pin = PinPolicy::from_env();
        // `off` ⇒ no authorial code surface: skip exec/wait entirely. `optional`
        // and `only` register + pin them; `only` additionally hides the flat
        // builtins from the wire array (below), making exec/wait the surface.
        if code_mode.enables_code_surface() {
            // Domain bindings (code-mode-cell-dsl.md §5): cell-only constructs
            // projected as namespace globals (`code.*`). They join the callable
            // set + seam — the surface ToolFilter still gates them by canonical
            // name — but never the flat wire registry: a binding exists only
            // inside cells.
            cm_callable.extend(
                crate::bindings::binding_tools()
                    .into_iter()
                    .filter(|t| tool_filter.permits(t.name())),
            );
            let cm_seam = services.tool_or_insert_with(|| {
                Arc::new(crate::capabilities::HostTools::new(
                    cm_callable.clone(),
                    cx.clone(),
                ))
            });
            builtins.extend(crate::code_mode::code_mode_tools(
                &cm_callable,
                cm_seam,
                code_mode,
                &crate::bindings::namespace_descriptions(),
            ));
            pin.also_pin(bro_code_mode::PUBLIC_TOOL_NAME);
            pin.also_pin(bro_code_mode::WAIT_TOOL_NAME);
        }
        if fleet {
            pin.also_pin(crate::report::REPORT_TOOL);
        }

        // Structured output: when an output schema is supplied, register a
        // synthetic `final_result` tool whose input_schema IS the output schema,
        // pinned into the wire surface. The agent loop detects a call to this
        // tool, captures its arguments as the structured result, and terminates.
        let output_schema: Option<String> = cli
            .output_schema
            .clone()
            .or_else(|| std::env::var("BRO_HARNESS_OUTPUT_SCHEMA").ok());
        let output_schema = output_schema
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok());
        if let Some(ref schema) = output_schema {
            builtins.push(Arc::new(FinalResultTool::new(schema.clone())));
            pin.also_pin(FINAL_RESULT_TOOL);
        }

        let reg = Registry::with_options(
            builtins,
            mcp_out_box,
            &pin,
            &tool_filter,
            code_mode.defers_builtins(),
        );
        validate_tool_arg_defaults(&cx.tool_arg_defaults, &reg);

        let base_opts = TurnOpts {
            base_instructions: Some(transport::base_instructions_for(&model)),
            model,
            max_tokens,
            system: SystemPrompt::default(),
            effort: cli.effort.clone(),
            web_search,
            service_tier: cli
                .service_tier
                .clone()
                .or(restored_service_tier)
                .or_else(|| std::env::var("BRO_HARNESS_SERVICE_TIER").ok()),
        };

        let emitter = make_emitter(store.id.clone(), callback, Some(event_log.clone()));
        let compaction = crate::compaction::CompactionPolicy::from_env();
        let compact_threshold = compaction.threshold(&base_opts.model);
        let tool_result_cap = crate::bound::cap_bytes();
        let dump_dir = crate::bound::dump_dir();

        // Timestamp the session boundary in the sidecar log. `provider` is the
        // daemon's dispatch provider when riding in-process
        // (`BRO_HARNESS_PROVIDER` in the per-session env); absent for the
        // standalone binary, where the transcript adapter falls back to
        // transport+model inference.
        event_log.append_milestone(
            if restored_snapshot {
                "session_resume"
            } else {
                "session_start"
            },
            &store.id,
            json!({
                "transport": tx.name(),
                "model": base_opts.model,
                "cwd": root.to_string_lossy(),
                "provider": transport::session_var("BRO_HARNESS_PROVIDER"),
            }),
        );

        Ok(Self {
            tx,
            reg,
            _services: services,
            code_mode,
            cx,
            reference_context_item,
            hooks,
            scoped_project_docs,
            emitter,
            base_opts,
            explicit_system,
            user_instructions,
            max_turns,
            compaction,
            compact_threshold,
            tool_result_cap,
            dump_dir,
            strategy,
            dispatch,
            store,
            event_log,
            prior_side,
            todos,
            lsp_baselines,
            lsp_pool: bro_lsp::SessionPool::new(bro_lsp::LspConfig::default()),
            lsp_documents: BTreeMap::new(),
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
            pending_input_estimate: 0,
            tail_nudge: None,
            output_schema,
        })
    }

    fn session_id(&self) -> &str {
        self.emitter.session_id()
    }

    /// Shared handle to the sidecar event log, for auxiliary emitters
    /// (control responses, stdin replay) created outside `build`.
    fn event_log(&self) -> Arc<EventLog> {
        self.event_log.clone()
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
        self.event_log.append_milestone(
            "compaction_start",
            self.emitter.session_id(),
            json!({"reason": "manual"}),
        );
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
                self.reference_context_item = None;
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
        let mut pending_prompt = Some(prompt);
        let prompt_estimate = est_tokens(prompt);

        // Timestamp the user turn in the sidecar log. The protocol stream
        // only carries user text when `--replay-user-messages` is on, so the
        // loop logs the authoritative turn itself, in envelope shape, so the
        // log stays a single uniform stream for postmortems and indexing.
        self.event_log.append_event(&json!({
            "type": "user",
            "session_id": self.session_id(),
            "message": {
                "role": "user",
                "content": [{"type": "text", "text": prompt}],
            },
        }));

        let mut final_text = String::new();
        // The TERMINAL step's text, tracked separately from `final_text`:
        // `final_text` keeps the last NON-EMPTY text of the whole turn (it is
        // the result payload), so it cannot detect a model that ends on an
        // output-free step — any earlier narration masks it (gap-aa032081).
        let mut last_step_text = String::new();
        // One-shot guard for the empty-output nudge below.
        let mut empty_output_nudged = false;
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

            let tool_specs = self.reg.wire_specs();

            // Compact before composing when the projected next prompt crosses the
            // model's window threshold. "Projected" = last observed input plus an
            // estimate of items appended since (tool results, mid-turn inputs, the
            // new user message), so an appended item that would overflow the next
            // request triggers compaction *before* it's sent. Tools are forwarded
            // so the server-side compaction path (brodex) can faithfully process
            // tool-call history.
            let pending_prompt_estimate = pending_prompt
                .as_ref()
                .map(|_| prompt_estimate)
                .unwrap_or_default();
            let projected_tokens = self
                .last_prompt_tokens
                .saturating_add(self.pending_input_estimate)
                .saturating_add(pending_prompt_estimate);
            if let Some(thresh) = self.compact_threshold
                && projected_tokens > thresh
            {
                self.event_log.append_milestone(
                    "compaction_start",
                    self.emitter.session_id(),
                    json!({"reason": "auto", "projected_tokens": projected_tokens}),
                );
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
                        self.reference_context_item = None;
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("compaction failed: {e:#}"),
                }
            }
            if let Some(prompt) = pending_prompt.take() {
                self.prepare_context_for_user_turn();
                self.push_user_text_raw(prompt);
            }
            let mut sys = compose_system(
                &self.system_sections(),
                &self.reg,
                self.output_schema.is_some(),
            );
            if let Some(t) = self.tail_nudge.take() {
                let v = sys.volatile.get_or_insert_with(String::new);
                if !v.is_empty() {
                    v.push('\n');
                }
                v.push_str(&t);
            }
            // Per-turn directives ride the volatile lane AFTER the existing
            // channels (structured-output reminder, tail nudges) — design §8.
            // On openai-chat after-tool turns the transport folds the volatile
            // tail into the leading system block (Mistral forbids
            // system-after-tool); everywhere else this is the uncached
            // trailing slot, late relative to the task.
            if let Some(per_turn) = self.dispatch.per_turn_text() {
                let v = sys.volatile.get_or_insert_with(String::new);
                if !v.is_empty() {
                    v.push('\n');
                }
                v.push_str(&per_turn);
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
                self.tx.normalize_for_prompt();
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
                        self.event_log.append_milestone(
                            "compaction_start",
                            self.emitter.session_id(),
                            json!({"reason": "overflow"}),
                        );
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
                                self.emitter.compact_boundary(
                                    "overflow",
                                    self.last_prompt_tokens,
                                    summary.len(),
                                );
                                self.reference_context_item = None;
                            }
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
            last_step_text = out.text.clone();

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
                // Carry the step's stop_reason + usage on the persisted
                // assistant event (gap-dab30623): same source of truth as the
                // suspicious-turn-end diagnostics (`out.stop` / `out.usage`),
                // but recorded per step so a max_tokens cut mid-session is
                // visible in events.jsonl without waiting for termination.
                self.emitter.assistant_message(
                    assistant_content,
                    Some(&out.stop),
                    Some(&out.usage),
                );
            }

            let has_tool_work = out.stop == StopReason::ToolCalls && !out.tool_calls.is_empty();
            let wants_follow_up = out.end_turn == Some(false);
            if !has_tool_work && !wants_follow_up {
                // Empty-output stop: the model ended its response with NO text
                // and NO tool calls — e.g. an output cap hit mid-thinking, or a
                // reasoning model that burned the response on a thinking block.
                // Breaking here would terminate the session as a clean success
                // with stale earlier narration as the result (gap-aa032081).
                // Nudge once for a real final answer before accepting the stop.
                if out.text.trim().is_empty() && !empty_output_nudged {
                    empty_output_nudged = true;
                    tracing::warn!(
                        stop = ?out.stop,
                        thinking_len = out.thinking.len(),
                        "model ended step with no text and no tool calls; nudging once"
                    );
                    let nudge = "Your previous response contained no visible output (no text, \
                                 no tool calls). Continue now: produce your final answer, or \
                                 proceed with tool calls.";
                    self.event_log.append_event(&json!({
                        "type": "user",
                        "session_id": self.session_id(),
                        "message": {
                            "role": "user",
                            "content": [{"type": "text", "text": nudge}],
                        },
                    }));
                    self.push_user_text_raw(nudge);
                    continue;
                }
                break if out.stop == StopReason::ToolCalls {
                    "tool_calls_empty"
                } else {
                    "model_stop"
                };
            }
            if !has_tool_work {
                continue;
            }

            // Structured output interception: when an output schema is active and
            // the model called `final_result`, capture the first call's arguments as
            // the structured result, emit a synthetic tool_result back, and terminate
            // the turn cleanly. The model should only call this once; ignore
            // subsequent calls if it fires multiple times.
            if self.output_schema.is_some() {
                if let Some(fr) = out
                    .tool_calls
                    .iter()
                    .find(|tc| tc.name == FINAL_RESULT_TOOL)
                {
                    let structured = fr.args.clone();
                    tracing::info!("final_result captured; terminating turn");
                    // Emit a tool_result so the transport buffer stays valid
                    // (every tool_use gets a matching result).
                    let fr_result = transport::ToolResult {
                        id: fr.id.clone(),
                        content: serde_json::to_string(&structured).unwrap_or_default(),
                        is_error: false,
                    };
                    self.emitter.tool_results(std::slice::from_ref(&fr_result));
                    // Pad any sibling tool calls that were NOT final_result with
                    // interrupted markers so the buffer stays balanced.
                    let mut padding: Vec<transport::ToolResult> = Vec::new();
                    for tc in &out.tool_calls {
                        if tc.name != FINAL_RESULT_TOOL {
                            padding.push(transport::ToolResult {
                                id: tc.id.clone(),
                                content: INTERRUPTED_TOOL_RESULT.to_string(),
                                is_error: true,
                            });
                        }
                    }
                    if !padding.is_empty() {
                        self.emitter.tool_results(&padding);
                    }
                    let mut transport_results = Vec::with_capacity(1 + padding.len());
                    transport_results.push(fr_result);
                    transport_results.extend(padding);
                    self.tx.push_tool_results(transport_results);
                    // Emit the structured result as the final assistant result.
                    self.emitter.result(
                        &serde_json::to_string(&structured).unwrap_or_default(),
                        &self.total_usage,
                        self.turns,
                        None,
                        None,
                        self.compact_threshold,
                    );
                    // Turn-boundary event-log drain (see end of user_turn).
                    let log = self.event_log.clone();
                    let _ = tokio::task::spawn_blocking(move || log.flush_blocking()).await;
                    return Ok(());
                }
            }

            // Dispatch tool calls. Read-only tools (per their annotation) run
            // CONCURRENTLY; mutating tools run serially after them. Serializing
            // mutators preserves the edit-sink + interrupt invariants and mirrors
            // codex's RwLock gate — read = shared/parallel, write = exclusive
            // (`codex-rs/core/src/tools/parallel.rs`). On interrupt, every
            // not-yet-resolved call is padded with an interrupted marker so the
            // assistant(tool_use) message keeps a matching tool_result.
            let call_count = out.tool_calls.len();
            let mut raw: Vec<Option<(String, bool)>> = (0..call_count).map(|_| None).collect();
            last_tool_results.clear();
            let mut interrupted = false;

            // Phase 1 — concurrent dispatch of read-only tools. They record no
            // edits and touch no `&mut self` state, so overlapping them is safe
            // and cuts latency on batches of reads (file_read/glob/search/…).
            {
                let read_idx: Vec<usize> = (0..call_count)
                    .filter(|&i| self.reg.read_only(&out.tool_calls[i].name))
                    .collect();
                if !read_idx.is_empty() {
                    tracing::info!(
                        parallel = read_idx.len(),
                        "dispatch (read-only, concurrent)"
                    );
                    let reg = &self.reg;
                    let cx = &self.cx;
                    let calls = &out.tool_calls;
                    let futs = read_idx.into_iter().map(|i| {
                        let tc = &calls[i];
                        async move { (i, reg.dispatch(&tc.name, tc.args.clone(), cx).await) }
                    });
                    tokio::select! {
                        biased;
                        _ = cancel.changed() => { interrupted = true; }
                        done = futures_util::future::join_all(futs) => {
                            for (i, res) in done {
                                raw[i] = Some(res.into_content());
                            }
                        }
                    }
                }
            }

            // Phase 2 — serial dispatch of every still-unresolved (mutating)
            // call, interruptible between calls.
            if !interrupted {
                for (i, tc) in out.tool_calls.iter().enumerate() {
                    if raw[i].is_some() {
                        continue;
                    }
                    tracing::info!(tool = %tc.name, "dispatch");
                    tokio::select! {
                        biased;
                        _ = cancel.changed() => { interrupted = true; break; }
                        res = self.reg.dispatch(&tc.name, tc.args.clone(), &self.cx) => {
                            raw[i] = Some(res.into_content());
                        }
                    }
                }
            }

            // Assemble results in tool-call order: bound oversized output, run
            // result hooks. Diagnostics are deferred to the single batch-boundary
            // pass below — the per-edit window-0 drain could not attribute edits
            // under concurrent dispatch or V8 code-mode cells.
            let mut results: Vec<transport::ToolResult> = Vec::with_capacity(call_count);
            for (i, tc) in out.tool_calls.iter().enumerate() {
                let Some((content, is_error)) = raw[i].take() else {
                    continue;
                };
                // Spill an oversized result to disk and inline a head + rider,
                // uniformly across builtin and MCP tools (§2.3).
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
                for n in self.hooks.on_tool_result(tc, &result) {
                    match n.delivery {
                        Delivery::Rider => result.content.push_str(&n.rider_block()),
                        Delivery::SystemTail => self.tail_nudge = Some(n.message),
                    }
                }
                if !result.is_error
                    && let Some(rider) = self.scoped_project_docs.rider_for_tool_call(
                        &self.cx.root,
                        &tc.name,
                        &tc.args,
                    )
                {
                    result.content.push_str(&rider);
                }
                last_tool_results.push(tool_result_trace(tc, &result));
                results.push(result);
            }

            // Batch-boundary diagnostics: one analyzer pass over every edit this
            // dispatch round produced, attached to the last result so it rides
            // back with the batch. Replaces the per-tool window-0 drain (which
            // could not attribute edits under concurrent dispatch / V8 cells).
            if let Some(last) = results.last_mut() {
                self.append_edit_diagnostics(&mut last.content).await;
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
            }

            self.emitter.tool_results(&results);
            self.pending_input_estimate = self
                .pending_input_estimate
                .saturating_add(est_tool_results(&results));
            self.tx.push_tool_results(results);
            if interrupted {
                break "interrupted_dispatch";
            }
            self.drain_mid_turn_user_inputs(&mid_turn_user_inputs);
            self.hooks.tick();
        };

        // An interrupted turn (cancelled model call, or cancelled tool dispatch)
        // leaves the buffer ending on a user-role message with no assistant
        // reply. Repair alternation now so the next turn — a steer, or a
        // `--resume` continuation — does not stack two user messages and 400.
        // A cancelled model call also drops the run_turn future mid-stream,
        // which would discard the usage accumulated for the in-flight
        // segment; recover it from the transport so the session total reflects
        // the tokens the underlying provider actually charged.
        if matches!(break_reason, "cancelled" | "interrupted_dispatch") {
            let partial = self.tx.take_interrupted_usage();
            if partial.input_tokens > 0
                || partial.output_tokens > 0
                || partial.cached_input_tokens > 0
                || partial.cache_creation_input_tokens > 0
            {
                self.total_usage.add(&partial);
                if partial.input_tokens > 0
                    || partial.cached_input_tokens > 0
                    || partial.cache_creation_input_tokens > 0
                {
                    self.last_prompt_tokens = partial.total_input_tokens();
                    self.pending_input_estimate = 0;
                }
            }
            self.tx.note_interrupted();
        }

        let turn_end = self.turn_end_diagnostics(
            break_reason,
            last_model_stop.as_ref(),
            last_model_tool_call_count,
            turn_steps,
            &last_tool_results,
            // The TERMINAL step's text — `final_text` would mask an
            // empty-output stop behind earlier narration (gap-aa032081).
            &last_step_text,
        );
        tracing::info!(turn_end = %turn_end, "turn ending");
        let suspicious = turn_end["suspicious"].as_bool().unwrap_or(false);
        if suspicious {
            tracing::warn!(turn_end = %turn_end, "suspicious turn end");
            self.emitter.turn_end_diagnostics(turn_end.clone());
        }

        if matches!(break_reason, "cancelled" | "interrupted_dispatch") {
            self.emitter.result_interrupted(
                &final_text,
                &self.total_usage,
                self.turns,
                self.compact_threshold,
            );
        } else {
            self.emitter.result(
                &final_text,
                &self.total_usage,
                self.turns,
                None,
                suspicious.then_some(&turn_end),
                self.compact_threshold,
            );
        }
        // Drain the sidecar event-log writer at the turn boundary — bounds
        // the crash-durability gap to the current turn while keeping
        // per-event appends off the runtime workers (event_log.rs).
        let log = self.event_log.clone();
        let _ = tokio::task::spawn_blocking(move || log.flush_blocking()).await;
        Ok(())
    }

    #[cfg(test)]
    fn push_user_text(&mut self, prompt: &str) {
        self.emit_initial_context_if_needed();
        self.push_user_text_raw(prompt);
    }

    fn push_user_text_raw(&mut self, prompt: &str) {
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

    /// Strategy-routed sections for the stable system slot. Codex-shaped:
    /// persona + standing directives only (memory/scope/pins ride the
    /// contextual-user lane). Vibe-shaped: memory, environment, scope, and
    /// pins additionally fold into the leading system block, rebuilt in place
    /// per request (vibe's `update_system_prompt` shape) — nothing
    /// context-shaped enters the user lane on that strategy.
    fn system_sections(&self) -> SystemSections {
        let mut sections = SystemSections {
            explicit: self.explicit_system.clone(),
            persona: self.dispatch.persona().map(str::to_string),
            standing: self.dispatch.standing_text(),
            ..SystemSections::default()
        };
        if !self.strategy.context_rides_user_lane() {
            sections.memory = self
                .user_instructions
                .as_ref()
                .map(crate::context::ContextualUserFragment::render);
            sections.environment = Some(crate::context::ContextualUserFragment::render(
                &crate::context::EnvironmentContext::from_tool_cx(&self.cx),
            ));
            sections.scope = self.dispatch.scope_render();
            sections.pins = self.dispatch.pins_render();
        }
        sections
    }

    fn prepare_context_for_user_turn(&mut self) {
        if self.reference_context_item.is_none() {
            self.emit_initial_context_if_needed();
        } else if self.strategy.context_rides_user_lane() {
            self.emit_environment_context_diff_if_needed();
            self.emit_dispatch_context_changes_if_needed();
        } else {
            // Vibe-shaped: the leading system rebuild carries
            // environment/scope/pins; advance the baseline silently so
            // side-state stays current.
            let env = crate::context::EnvironmentContext::from_tool_cx(&self.cx);
            self.reference_context_item = Some(env.to_turn_context_item());
        }
    }

    fn emit_initial_context_if_needed(&mut self) {
        if self.reference_context_item.is_some() {
            return;
        }
        let env = crate::context::EnvironmentContext::from_tool_cx(&self.cx);
        // The emitter is strategy-aware (design §5, review round 2 blocker):
        // on the vibe-shaped strategy memory/environment/scope/pins resolve to
        // the stable system slot, so the initial-context emitter contributes
        // NOTHING to the user lane.
        if self.strategy.context_rides_user_lane() {
            // Turn-1 contextual user message ordering (codex order):
            // UserInstructions (AGENTS.md) → scope → pins → environment LAST.
            let mut sections = Vec::new();
            if let Some(instructions) = &self.user_instructions {
                sections.push(crate::context::ContextualUserFragment::render(instructions));
            }
            if let Some(scope) = self.dispatch.scope_render() {
                self.dispatch.emitted_scope = Some(scope.clone());
                sections.push(scope);
            }
            if let Some(pins) = self.dispatch.pins_render() {
                self.dispatch.emitted_pins = Some(pins.clone());
                sections.push(pins);
            }
            sections.push(crate::context::ContextualUserFragment::render(&env));
            if let Some(message) = crate::context::build_contextual_user_message(sections) {
                let added_tokens = message
                    .text_blocks
                    .iter()
                    .map(|section| est_tokens(section))
                    .fold(0u64, u64::saturating_add);
                self.tx.push_user_text_blocks(message.text_blocks);
                self.pending_input_estimate =
                    self.pending_input_estimate.saturating_add(added_tokens);
            }
        }
        self.reference_context_item = Some(env.to_turn_context_item());
    }

    /// Re-emit scope/pins fragments when the current dispatch context differs
    /// from the last-emitted baselines (resume with a changed scope, pin
    /// update). No current scope ⇒ nothing emitted and the baseline survives
    /// for future comparison (design §4/§7).
    fn emit_dispatch_context_changes_if_needed(&mut self) {
        let mut sections: Vec<String> = Vec::new();
        if let Some(scope) = self.dispatch.scope_render()
            && self.dispatch.emitted_scope.as_deref() != Some(scope.as_str())
        {
            self.dispatch.emitted_scope = Some(scope.clone());
            sections.push(scope);
        }
        if let Some(pins) = self.dispatch.pins_render()
            && self.dispatch.emitted_pins.as_deref() != Some(pins.as_str())
        {
            self.dispatch.emitted_pins = Some(pins.clone());
            sections.push(pins);
        }
        if let Some(message) = crate::context::build_contextual_user_message(sections) {
            let added_tokens = message
                .text_blocks
                .iter()
                .map(|section| est_tokens(section))
                .fold(0u64, u64::saturating_add);
            self.tx.push_user_text_blocks(message.text_blocks);
            self.pending_input_estimate = self.pending_input_estimate.saturating_add(added_tokens);
        }
    }

    fn emit_environment_context_diff_if_needed(&mut self) {
        let Some(before) = self.reference_context_item.clone() else {
            return;
        };
        let env = crate::context::EnvironmentContext::from_tool_cx(&self.cx);
        if let Some(delta) =
            crate::context::EnvironmentContextDelta::from_turn_context_item(&before, &env)
        {
            let rendered = crate::context::ContextualUserFragment::render(&delta);
            if let Some(message) = crate::context::build_contextual_user_message(vec![rendered]) {
                let added_tokens = message
                    .text_blocks
                    .iter()
                    .map(|section| est_tokens(section))
                    .fold(0u64, u64::saturating_add);
                self.tx.push_user_text_blocks(message.text_blocks);
                self.pending_input_estimate =
                    self.pending_input_estimate.saturating_add(added_tokens);
            }
        }
        // Match Codex's runtime baseline advance: even a shell-only change
        // emits no model-visible delta, but the in-memory side-state baseline
        // still reflects the current turn environment.
        self.reference_context_item = Some(env.to_turn_context_item());
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
            // Log the steer like the turn-start user log above: the event log
            // is THE transcript (the fleet zoom renders it and reconciles
            // queued-steer echoes against it), so an operator turn injected
            // mid-turn must appear in it at the position the model saw it.
            self.event_log.append_event(&json!({
                "type": "user",
                "session_id": self.session_id(),
                "message": {
                    "role": "user",
                    "content": [{"type": "text", "text": prompt}],
                },
            }));
            self.push_user_text_raw(&prompt);
        }
    }

    fn turn_end_diagnostics(
        &self,
        break_reason: &str,
        last_model_stop: Option<&StopReason>,
        last_model_tool_call_count: usize,
        turn_steps: u64,
        last_tool_results: &[Value],
        // Text of the TERMINAL model step only — not the session-accumulated
        // result text. Using the accumulated text here masks empty-output
        // stops behind any earlier narration (gap-aa032081).
        last_turn_text: &str,
    ) -> Value {
        let shell_ids = self.cx.shell_sessions.lock().unwrap().ids();
        let last_tool_running = last_tool_results
            .iter()
            .any(|v| v["running"].as_bool() == Some(true));
        let outstanding_async = !shell_ids.is_empty() || last_tool_running;

        // Empty-output stop: the model itself ended the turn (not max_turns /
        // cancel / interrupt) having produced no assistant text AND no tool
        // calls. This is the classic spurious-stop signature — the model
        // returned nothing and the turn silently terminated — which the
        // outstanding-async heuristic above does not catch. A `tool_calls_empty`
        // break is abnormal by construction (stop=tool_calls yet zero calls);
        // for it we flag regardless of text.
        let produced_text = !last_turn_text.trim().is_empty();
        let model_ended = matches!(break_reason, "model_stop" | "tool_calls_empty");
        let empty_output_stop = model_ended && last_model_tool_call_count == 0 && !produced_text;

        let mut suspicion_reasons: Vec<&str> = Vec::new();
        if !shell_ids.is_empty() {
            suspicion_reasons.push("outstanding_shell_sessions");
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
            "last_turn_text_len": last_turn_text.len(),
            "produced_text": produced_text,
            "empty_output_stop": empty_output_stop,
            "outstanding_shell_sessions": {
                "count": shell_ids.len(),
                "ids": shell_ids,
            },
            "last_tool_results": last_tool_results,
            "suspicion_reasons": suspicion_reasons,
            "suspicious": suspicious,
        })
    }

    /// Batch-boundary diagnostics seam: drain the edit sink accumulated over the
    /// whole dispatch round, run the analyzer against the edited files, and
    /// append a diagnostics rider to `content` — synchronously, before control
    /// returns to the model. Invoked once per dispatch batch (not per tool), so
    /// it attributes correctly under concurrent dispatch and V8 code-mode cells,
    /// where a single drain may span many edits. A no-op when no edits were
    /// recorded. A diagnostics failure is logged and swallowed: it must never
    /// break the dispatch loop.
    async fn append_edit_diagnostics(&mut self, content: &mut String) {
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

    /// Serialize session state to a JSON string without performing I/O.
    /// Callers should write the returned body to `store_path()` via
    /// `tokio::task::spawn_blocking` to keep the write off the runtime.
    fn persist_body(&self) -> Result<String> {
        serde_json::to_string(&json!({
            "transport": self.tx.name(),
            "model": &self.base_opts.model,
            "code_mode": self.code_mode.as_str(),
            "service_tier": self.base_opts.service_tier.as_deref(),
            "snapshot": self.tx.snapshot(),
            "side": self.side_state(),
        }))
        .context("serialize session")
    }

    fn store_path(&self) -> &std::path::PathBuf {
        self.store.store_path()
    }

    fn side_state(&self) -> Value {
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
        side["lsp_baselines"] = self.lsp_baselines.to_side();
        side["reference_context"] = self
            .reference_context_item
            .as_ref()
            .map(crate::context::TurnContextItem::to_side)
            .unwrap_or(Value::Null);
        side["dispatch_context"] = self.dispatch.context_to_side();
        side["dispatch_emitted"] = self.dispatch.emitted_to_side();
        side
    }
}

fn reference_context_item_for_restore(
    restored_snapshot: bool,
    restored_reference_context: Option<crate::context::TurnContextItem>,
    cx: &ToolCx,
) -> Option<crate::context::TurnContextItem> {
    if !restored_snapshot {
        return None;
    }
    restored_reference_context.or_else(|| {
        Some(crate::context::EnvironmentContext::from_tool_cx(cx).to_turn_context_item())
    })
}

fn load_tool_arg_defaults(
    explicit: Option<BTreeMap<String, String>>,
    cli_json: Option<&str>,
) -> Result<bro_tools::ToolArgDefaults> {
    let raw = match explicit {
        Some(map) => map,
        None => match cli_json {
            Some(raw) => parse_tool_arg_defaults_json(raw)
                .context("parse --additional-context as JSON string map")?,
            None => match std::env::var("BRO_HARNESS_TOOL_DEFAULTS") {
                Ok(raw) if !raw.trim().is_empty() => parse_tool_arg_defaults_json(&raw)
                    .context("parse BRO_HARNESS_TOOL_DEFAULTS as JSON string map")?,
                _ => BTreeMap::new(),
            },
        },
    };
    bro_tools::ToolArgDefaults::parse_map(raw)
        .map_err(anyhow::Error::msg)
        .context("parse tool arg default table")
}

/// Host-supplied shell env overlay: explicit in-process map > `--shell-env`
/// JSON > `BRO_HARNESS_SHELL_ENV`. Same precedence ladder as the tool-arg
/// default table; values are plain env pairs, no grammar.
fn load_shell_env(
    explicit: Option<BTreeMap<String, String>>,
    cli_json: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    match explicit {
        Some(map) => Ok(map),
        None => match cli_json {
            Some(raw) => {
                parse_tool_arg_defaults_json(raw).context("parse --shell-env as JSON string map")
            }
            None => match std::env::var("BRO_HARNESS_SHELL_ENV") {
                Ok(raw) if !raw.trim().is_empty() => parse_tool_arg_defaults_json(&raw)
                    .context("parse BRO_HARNESS_SHELL_ENV as JSON string map"),
                _ => Ok(BTreeMap::new()),
            },
        },
    }
}

fn parse_tool_arg_defaults_json(raw: &str) -> Result<BTreeMap<String, String>> {
    serde_json::from_str::<BTreeMap<String, String>>(raw)
        .context("expected a JSON object with string keys and string values")
}

fn validate_tool_arg_defaults(defaults: &bro_tools::ToolArgDefaults, reg: &Registry) {
    if defaults.is_empty() {
        return;
    }
    let schemas = reg.schemas();
    for warning in
        defaults.validation_warnings(schemas.iter().map(|(name, schema)| (name.as_str(), schema)))
    {
        tracing::warn!(warning = %warning, "tool arg default schema validation warning");
        eprintln!("BRO_HARNESS_TOOL_DEFAULTS warning: {warning}");
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

/// Strategy-routed sections feeding the stable system slot
/// (design/bro-harness/dispatch-prompt-slots.md §5). `explicit`, `persona`,
/// and `standing` apply under every strategy; `memory`, `environment`,
/// `scope`, and `pins` are filled only by the vibe-shaped strategy, where
/// those classes fold into the leading system message instead of the
/// contextual-user lane.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct SystemSections {
    explicit: Option<String>,
    persona: Option<String>,
    standing: Option<String>,
    memory: Option<String>,
    environment: Option<String>,
    scope: Option<String>,
    pins: Option<String>,
}

/// Compose the effective system prompt as a cache-stable prefix plus a volatile
/// tail. See the transport `SystemPrompt` docs.
///
/// Stable ordering (both strategies; base instructions render before all of
/// this, transport-side): explicit `--system-prompt` override → persona →
/// standing directives → memory → pinned-tools → environment → scope → pins.
/// The per-resume-mutable sections (scope/pins) sit at the suffix so the
/// prefix stays byte-identical across leading-block rebuilds on the chat
/// lane (cache vs salience trade, design §5).
fn compose_system(
    sections: &SystemSections,
    reg: &Registry,
    has_structured_output: bool,
) -> SystemPrompt {
    fn push_part(parts: &mut Vec<String>, s: Option<&str>) {
        if let Some(s) = s
            && !s.trim().is_empty()
        {
            parts.push(s.trim_end().to_string());
        }
    }
    let mut parts: Vec<String> = Vec::new();
    push_part(&mut parts, sections.explicit.as_deref());
    push_part(&mut parts, sections.persona.as_deref());
    push_part(&mut parts, sections.standing.as_deref());
    push_part(&mut parts, sections.memory.as_deref());

    let pinned = reg.pinned();
    if !pinned.is_empty() {
        let mut block = String::new();
        block.push_str(
            "## Always-available tools\n\
             These are loaded and ready — prefer them for their purpose; do not search for them. \
             `tool_search` loads anything else on demand.\n",
        );
        for (name, desc) in pinned {
            block.push_str(&format!("- `{name}` — {desc}\n"));
        }
        if reg.contains("shell_poll") {
            block.push_str(
                "\nShell sessions: `shell_run` waits up to `yield_time_ms` for exit (default ~1s; \
                 `0` waits until exit/timeout). If `shell_run` or `shell_poll` returns `running=true`, \
                 the command is still active; call `shell_poll` with the returned `session_id` \
                 until `running=false` (use its `yield_time_ms` to wake later), or `shell_kill` \
                 if you are abandoning it.\n",
            );
        }
        parts.push(block.trim_end().to_string());
    }
    push_part(&mut parts, sections.environment.as_deref());
    push_part(&mut parts, sections.scope.as_deref());
    push_part(&mut parts, sections.pins.as_deref());
    let stable = parts.join("\n\n");

    let mut volatile = String::new();
    if has_structured_output {
        volatile.push_str(STRUCTURED_OUTPUT_INSTRUCTION);
        volatile.push('\n');
    }
    // The deferred-tool manifest is AMBIENT, not volatile: it changes only
    // when the loaded-tool set changes, so transports can deliver it
    // hash-gated instead of re-sending it as a fresh item every turn.
    let mut ambient = String::new();
    let manifest = reg.manifest();
    if !manifest.is_empty() {
        ambient.push_str(&format!(
            "## Additional tools ({} available, not yet loaded)\n\
             Call `tool_search(\"<keywords>\")` to search/load by purpose, or \
             `tool_search(\"select:name1,name2\")` if you already know exact names. \
             `tool_search` returns compact match metadata by default; pass \
             `include_schemas=true` only when the compact result is insufficient.\n",
            manifest.len()
        ));
        let preview_len = manifest.len().min(12);
        for (name, desc) in manifest.iter().take(preview_len) {
            ambient.push_str(&format!("- {name}: {desc}\n"));
        }
        if manifest.len() > preview_len {
            ambient.push_str(&format!(
                "- … {} more hidden; use `tool_search` keywords to discover/load them.\n",
                manifest.len() - preview_len
            ));
        }
    }

    SystemPrompt {
        stable: (!stable.is_empty()).then_some(stable),
        ambient: (!ambient.is_empty()).then_some(ambient),
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
    fn builtin_tools_include_yield_poll_shell_session_tools() {
        // Both fleet and non-fleet now use the same yield/poll shell tools
        // (the promise-only fleet shell was retired in favor of codex's pull
        // model). The shell-session family must always be present.
        let names: HashSet<_> = builtin_tools()
            .into_iter()
            .map(|tool| tool.name().to_string())
            .collect();

        assert!(names.contains("shell_run"));
        assert!(names.contains("shell_poll"));
        assert!(names.contains("shell_kill"));
        assert!(names.contains("shell_list"));
    }

    /// Session env (the in-process daemon's per-dispatch `env_overrides`) must
    /// beat process env for BRO_HARNESS_WEB_SEARCH, and the standalone-binary
    /// path (no session scope) still reads process env — mirrors the
    /// `session_var` contract exercised in `transport::session_env_tests`.
    #[tokio::test]
    async fn web_search_flag_prefers_session_env_over_process_env() {
        // SAFETY: this key is read/written only by this test (the other
        // web-search tests below never touch process env).
        unsafe { std::env::set_var("BRO_HARNESS_WEB_SEARCH", "1") };

        transport::with_session_env(
            std::collections::BTreeMap::from([(
                "BRO_HARNESS_WEB_SEARCH".to_string(),
                "0".to_string(),
            )]),
            async {
                assert!(
                    !web_search_enabled(),
                    "per-session opt-out must beat process env"
                );
            },
        )
        .await;

        // Outside any session scope → process env (standalone binary).
        assert!(web_search_enabled());

        // SAFETY: cleanup of this test's key.
        unsafe { std::env::remove_var("BRO_HARNESS_WEB_SEARCH") };
        // Absent everywhere → enabled by default.
        assert!(web_search_enabled());
    }

    #[tokio::test]
    async fn web_search_flag_session_value_semantics() {
        for (value, expected) in [
            ("0", false),
            ("false", false),
            ("FALSE", false),
            ("1", true),
        ] {
            transport::with_session_env(
                std::collections::BTreeMap::from([(
                    "BRO_HARNESS_WEB_SEARCH".to_string(),
                    value.to_string(),
                )]),
                async move {
                    assert_eq!(web_search_enabled(), expected, "value {value:?}");
                },
            )
            .await;
        }
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
        /// Return text immediately with a Responses-style follow-up signal.
        TextWithEndTurn(String, Option<bool>),
        /// Wait on the shared gate, then request a tool call.
        ToolCallAfterGate,
        /// Request two read-only `concurrent_probe` calls in one batch (to prove
        /// they dispatch concurrently).
        TwoReadProbes,
        /// Request the synthetic structured-output terminal tool.
        FinalResult,
        /// Request a test-only file_read call under a child directory.
        FileReadUnderChild,
        /// Await a gate that tests never release — to be cancelled by interrupt.
        Block,
    }

    #[derive(Clone, Default)]
    struct MockShared {
        pushed_users: Arc<Mutex<Vec<String>>>,
        pushed_tool_results: Arc<Mutex<Vec<Vec<transport::ToolResult>>>>,
        started: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
        compact_calls: Arc<AtomicUsize>,
        model_gate: Arc<Notify>,
        tool_started: Arc<AtomicUsize>,
        tool_gate: Arc<Notify>,
        /// Count of read-only probe calls that rendezvoused at the shared
        /// barrier — only reaches 2 if the batch ran concurrently (phase 1).
        rendezvous: Arc<AtomicUsize>,
        /// SystemPrompt observed by each run_turn call, for slot-routing
        /// assertions (volatile-lane ordering, stable composition).
        seen_systems: Arc<Mutex<Vec<SystemPrompt>>>,
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
        fn push_tool_results(&mut self, results: Vec<transport::ToolResult>) {
            self.shared
                .pushed_tool_results
                .lock()
                .unwrap()
                .push(results);
        }
        async fn run_turn(
            &mut self,
            _tools: &[transport::ToolSpec],
            opts: &TurnOpts,
            _sink: &dyn transport::TurnSink,
        ) -> Result<transport::TurnOutput> {
            self.shared.started.fetch_add(1, Ordering::SeqCst);
            self.shared
                .seen_systems
                .lock()
                .unwrap()
                .push(opts.system.clone());
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
                        end_turn: None,
                        usage: Usage::default(),
                    })
                }
                MockTurn::TwoReadProbes => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: String::new(),
                        thinking: String::new(),
                        tool_calls: vec![
                            transport::ToolCall {
                                id: "probe-1".into(),
                                name: "concurrent_probe".into(),
                                args: json!({}),
                            },
                            transport::ToolCall {
                                id: "probe-2".into(),
                                name: "concurrent_probe".into(),
                                args: json!({}),
                            },
                        ],
                        stop: StopReason::ToolCalls,
                        end_turn: None,
                        usage: Usage::default(),
                    })
                }
                MockTurn::FinalResult => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: String::new(),
                        thinking: String::new(),
                        tool_calls: vec![transport::ToolCall {
                            id: "final-1".into(),
                            name: FINAL_RESULT_TOOL.into(),
                            args: json!({"ok": true}),
                        }],
                        stop: StopReason::ToolCalls,
                        end_turn: None,
                        usage: Usage::default(),
                    })
                }
                MockTurn::FileReadUnderChild => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: String::new(),
                        thinking: String::new(),
                        tool_calls: vec![transport::ToolCall {
                            id: "read-1".into(),
                            name: "file_read".into(),
                            args: json!({"file_path": "crates/thing/src/lib.rs"}),
                        }],
                        stop: StopReason::ToolCalls,
                        end_turn: None,
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
                        end_turn: None,
                        usage: Usage::default(),
                    })
                }
                MockTurn::TextWithEndTurn(t, end_turn) => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: t,
                        thinking: String::new(),
                        tool_calls: vec![],
                        stop: StopReason::Done,
                        end_turn,
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

    struct FileReadTool;

    #[async_trait]
    impl bro_tools::Tool for FileReadTool {
        fn name(&self) -> &str {
            "file_read"
        }

        fn description(&self) -> &str {
            "test-only file read tool"
        }

        fn input_schema(&self) -> Value {
            json!({"type":"object","properties":{"file_path":{"type":"string"}}})
        }

        async fn call(&self, _input: Value, _cx: &ToolCx) -> bro_tools::ToolResult {
            bro_tools::ToolResult::Text("FILE-BODY".into())
        }
    }

    /// Read-only probe that rendezvouses at a shared 2-party barrier. Two of
    /// these in one batch can only both pass the barrier if they are dispatched
    /// concurrently; under serial dispatch the first waits alone until it times
    /// out, so `rendezvous` never reaches 2.
    struct ConcurrentProbe {
        shared: MockShared,
        barrier: Arc<tokio::sync::Barrier>,
    }

    #[async_trait]
    impl bro_tools::Tool for ConcurrentProbe {
        fn name(&self) -> &str {
            "concurrent_probe"
        }

        fn description(&self) -> &str {
            "test-only read-only concurrency probe"
        }

        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }

        async fn call(&self, _input: Value, _cx: &ToolCx) -> bro_tools::ToolResult {
            let rendezvoused =
                tokio::time::timeout(std::time::Duration::from_secs(2), self.barrier.wait())
                    .await
                    .is_ok();
            if rendezvoused {
                self.shared.rendezvous.fetch_add(1, Ordering::SeqCst);
            }
            bro_tools::ToolResult::Text(if rendezvoused { "rendezvous" } else { "alone" }.into())
        }

        fn annotations(&self) -> bro_tools::ToolAnnotations {
            bro_tools::ToolAnnotations {
                read_only: true,
                destructive: false,
            }
        }
    }

    fn mk_session(scripts: Vec<MockTurn>) -> (Session, MockShared) {
        let shared = MockShared::default();
        let mock = MockTransport {
            shared: shared.clone(),
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
        };
        let todos = Arc::new(Mutex::new(bro_tools::TodoList::default()));
        let cx = ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = format!("bh-test-{}-{}", std::process::id(), nanos);
        let session = Session {
            tx: Box::new(mock),
            _services: HarnessSessionServices::standalone(),
            code_mode: crate::code_mode::CodeMode::Optional,
            output_schema: None,
            reg: Registry::new(
                vec![
                    Arc::new(SlowTool {
                        shared: shared.clone(),
                    }),
                    Arc::new(ConcurrentProbe {
                        shared: shared.clone(),
                        barrier: Arc::new(tokio::sync::Barrier::new(2)),
                    }),
                    Arc::new(FileReadTool),
                ],
                vec![],
                &PinPolicy::from_env(),
                &mcp::ToolFilter::default(),
            ),
            cx,
            reference_context_item: None,
            hooks: HookEngine::from_env(NudgeLedger::from_side(&Value::Null)),
            scoped_project_docs: crate::project_doc::ScopedProjectDocs::default(),
            strategy: crate::context::dispatch::CompositionStrategy::CodexShaped,
            dispatch: crate::context::dispatch::DispatchState::default(),
            emitter: Emitter::new("test".into()),
            base_opts: TurnOpts {
                model: "m".into(),
                max_tokens: 8,
                base_instructions: None,
                system: SystemPrompt::default(),
                effort: None,
                web_search: false,
                service_tier: None,
            },
            explicit_system: None,
            user_instructions: None,
            max_turns: 50,
            compaction: crate::compaction::CompactionPolicy::from_env(),
            compact_threshold: None,
            tool_result_cap: 0,
            dump_dir: std::env::temp_dir(),
            store: SessionStore::open(Some(&id), None).unwrap(),
            event_log: Arc::new(EventLog::disabled()),
            prior_side: Value::Null,
            todos,
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

    async fn run_user_turn(session: &mut Session, prompt: &str) {
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        session
            .user_turn(prompt, cancel_rx, Arc::new(StdMutex::new(VecDeque::new())))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn final_result_tool_result_is_pushed_to_transport_before_return() {
        let (mut session, shared) = mk_session(vec![MockTurn::FinalResult]);
        session.output_schema = Some(json!({
            "type": "object",
            "properties": { "ok": { "type": "boolean" } },
            "required": ["ok"]
        }));

        run_user_turn(&mut session, "structured please").await;

        let pushed = shared.pushed_tool_results.lock().unwrap();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].len(), 1);
        assert_eq!(pushed[0][0].id, "final-1");
        assert_eq!(pushed[0][0].content, r#"{"ok":true}"#);
        assert!(!pushed[0][0].is_error);
    }

    #[tokio::test]
    async fn file_read_under_child_dir_attaches_scoped_project_doc_rider() {
        let root = std::env::temp_dir().join(format!(
            "bh-agents-rider-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let child = root.join("crates").join("thing");
        std::fs::create_dir_all(child.join("src")).unwrap();
        std::fs::write(child.join("AGENTS.md"), "CHILD-DOC").unwrap();
        std::fs::write(child.join("src").join("lib.rs"), "FILE-BODY").unwrap();

        let (mut session, shared) = mk_session(vec![MockTurn::FileReadUnderChild]);
        session.cx.root = root.canonicalize().unwrap();

        run_user_turn(&mut session, "read it").await;

        let pushed = shared.pushed_tool_results.lock().unwrap();
        assert_eq!(pushed.len(), 1);
        assert_eq!(pushed[0].len(), 1);
        assert_eq!(pushed[0][0].content.matches("CHILD-DOC").count(), 1);
        assert!(pushed[0][0].content.contains("<harness-project-docs>"));
        assert!(pushed[0][0].content.contains("FILE-BODY"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[tokio::test]
    async fn user_turn_tees_timestamped_events_into_sidecar_log() {
        let dir = std::env::temp_dir().join(format!(
            "bh-evlog-turn-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let log = Arc::new(EventLog::at_path(dir.join("test.events.jsonl")));

        let (mut session, _shared) = mk_session(vec![MockTurn::Text("answer".into())]);
        session.event_log = log.clone();
        session.emitter = Emitter::new("test".into()).with_event_log(log.clone());

        run_user_turn(&mut session, "what is up").await;

        let lines: Vec<Value> = std::fs::read_to_string(log.path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Every line carries a parseable RFC3339 ts wrapping an envelope event.
        for line in &lines {
            chrono::DateTime::parse_from_rfc3339(line["ts"].as_str().expect("ts"))
                .expect("rfc3339 ts");
        }
        let types: Vec<&str> = lines
            .iter()
            .map(|l| l["event"]["type"].as_str().unwrap())
            .collect();
        // The loop logs the user turn; the emitter tee logs the assistant turn
        // and the terminal result.
        assert_eq!(types.iter().filter(|t| **t == "user").count(), 1);
        assert!(types.contains(&"assistant"), "{types:?}");
        assert!(types.contains(&"result"), "{types:?}");
        let user = lines.iter().find(|l| l["event"]["type"] == "user").unwrap();
        assert_eq!(user["event"]["message"]["content"][0]["text"], "what is up");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn empty_output_stop_is_nudged_once_then_recovers() {
        // gap-aa032081: a model step that ends with NO text and NO tool calls
        // (e.g. an output cap hit mid-thinking) must not silently terminate
        // the session as success with stale text. The loop nudges once; here
        // the model recovers with a real answer on the retry.
        let (mut session, shared) = mk_session(vec![
            MockTurn::Text(String::new()),
            MockTurn::Text("recovered answer".into()),
        ]);

        run_user_turn(&mut session, "do the thing").await;

        assert_eq!(
            shared.started.load(Ordering::SeqCst),
            2,
            "empty-output stop should trigger exactly one retry"
        );
        let pushed = shared.pushed_users.lock().unwrap().clone();
        assert!(
            pushed.iter().any(|p| p.contains("no visible output")),
            "the nudge must reach the transport buffer: {pushed:?}"
        );
    }

    #[tokio::test]
    async fn empty_output_stop_nudges_only_once_then_breaks() {
        // If the model returns nothing AGAIN after the nudge, the turn must
        // end (no nudge loop) — and the terminal-turn-text detector flags it.
        let (mut session, shared) = mk_session(vec![
            MockTurn::Text(String::new()),
            MockTurn::Text(String::new()),
        ]);

        run_user_turn(&mut session, "do the thing").await;

        assert_eq!(
            shared.started.load(Ordering::SeqCst),
            2,
            "exactly one nudge retry, then the turn breaks"
        );
    }

    #[tokio::test]
    async fn read_only_tools_in_a_batch_dispatch_concurrently() {
        // The model emits two read-only `concurrent_probe` calls in one batch.
        // Each rendezvouses at a shared 2-party barrier; both can only pass if
        // they run concurrently (phase 1). Serial dispatch would leave the first
        // probe waiting alone until its timeout, so `rendezvous` would stay < 2.
        let (mut session, shared) =
            mk_session(vec![MockTurn::TwoReadProbes, MockTurn::Text("done".into())]);

        run_user_turn(&mut session, "go").await;

        assert_eq!(
            shared.rendezvous.load(Ordering::SeqCst),
            2,
            "both read-only probes must have run concurrently"
        );
    }

    #[test]
    fn first_user_push_emits_environment_context_and_baseline() {
        let (mut session, shared) = mk_session(vec![]);
        let expected_cwd = session.cx.root.to_string_lossy().into_owned();
        session.push_user_text("hello");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.len(), 2);
        assert!(pushed[0].starts_with("<environment_context>"));
        assert!(pushed[0].contains(&format!("<cwd>{expected_cwd}</cwd>")));
        assert_eq!(pushed[1], "hello");

        let baseline = session
            .reference_context_item
            .as_ref()
            .expect("baseline captured");
        assert_eq!(baseline.cwd, expected_cwd);
        assert!(baseline.current_date.is_some());
    }

    #[test]
    fn fresh_user_push_writes_reference_context_side_state() {
        let (mut session, _shared) = mk_session(vec![]);
        session.push_user_text("hello");

        let side = session.side_state();
        let persisted =
            crate::context::TurnContextItem::from_side(&side["reference_context"]).unwrap();

        assert_eq!(
            Some(&persisted),
            session.reference_context_item.as_ref(),
            "{side}"
        );
    }

    #[test]
    fn restored_reference_context_suppresses_emit_and_preserves_persisted_baseline() {
        let (mut session, shared) = mk_session(vec![]);
        let persisted = crate::context::TurnContextItem {
            cwd: "/persisted/baseline".into(),
            shell: Some("/bin/persisted-shell".into()),
            current_date: Some("2026-01-02".into()),
            timezone: Some("America/New_York".into()),
        };
        session.reference_context_item =
            reference_context_item_for_restore(true, Some(persisted.clone()), &session.cx);

        session.push_user_text("resume turn");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.as_slice(), &["resume turn".to_string()]);
        assert_eq!(session.reference_context_item, Some(persisted));
    }

    #[test]
    fn seeded_reference_context_suppresses_initial_context_emit() {
        let (mut session, shared) = mk_session(vec![]);
        let env = crate::context::EnvironmentContext::from_tool_cx(&session.cx);
        session.reference_context_item = Some(env.to_turn_context_item());

        session.push_user_text("resume turn");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.as_slice(), &["resume turn".to_string()]);
    }

    #[test]
    fn legacy_resumed_session_without_reference_context_gets_marker_baseline() {
        let (session, _shared) = mk_session(vec![]);

        let restored = reference_context_item_for_restore(true, None, &session.cx);

        assert!(restored.is_some());
        assert_eq!(restored.unwrap().cwd, session.cx.root.to_string_lossy());
    }

    #[tokio::test]
    async fn compaction_clear_persists_null_and_next_turn_reinjects_full_context() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("ok".into())]);
        let env = crate::context::EnvironmentContext::from_tool_cx(&session.cx);
        session.reference_context_item = Some(env.to_turn_context_item());
        session.user_instructions = Some(crate::context::UserInstructions {
            directory: "/repo".into(),
            text: "AGENTS_AFTER_COMPACT".into(),
            loaded_paths: Vec::new(),
        });

        session.compact_manual().await.unwrap();

        assert_eq!(session.reference_context_item, None);
        assert_eq!(session.side_state()["reference_context"], Value::Null);

        run_user_turn(&mut session, "after compact").await;

        let pushed = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(pushed.len(), 2, "{pushed:?}");
        assert!(pushed[0].contains("# AGENTS.md instructions"), "{pushed:?}");
        assert!(pushed[0].contains("AGENTS_AFTER_COMPACT"), "{pushed:?}");
        assert!(pushed[0].contains("<environment_context>"), "{pushed:?}");
        assert!(pushed[0].contains("<cwd>"), "{pushed:?}");
        assert!(pushed[0].contains("<current_date>"), "{pushed:?}");
        assert!(pushed[0].contains("<timezone>"), "{pushed:?}");
        assert_eq!(pushed[1], "after compact");
        assert!(
            session.reference_context_item.is_some(),
            "full re-inject should re-establish the baseline"
        );
    }

    #[tokio::test]
    async fn unchanged_environment_emits_no_second_turn_diff() {
        let (mut session, shared) = mk_session(vec![
            MockTurn::Text("one".into()),
            MockTurn::Text("two".into()),
        ]);

        run_user_turn(&mut session, "one").await;
        run_user_turn(&mut session, "two").await;

        let pushed = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(pushed.len(), 3, "{pushed:?}");
        assert!(pushed[0].starts_with("<environment_context>"), "{pushed:?}");
        assert_eq!(pushed[1], "one");
        assert_eq!(pushed[2], "two");
    }

    #[tokio::test]
    async fn cwd_change_emits_one_field_environment_delta_and_updates_baseline() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("ok".into())]);
        let current = crate::context::EnvironmentContext::from_tool_cx(&session.cx);
        session.reference_context_item = Some(crate::context::TurnContextItem {
            cwd: "/old/cwd".into(),
            shell: current.shell.clone(),
            current_date: current.current_date.clone(),
            timezone: current.timezone.clone(),
        });

        run_user_turn(&mut session, "turn").await;

        let pushed = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(pushed.len(), 2, "{pushed:?}");
        let delta = &pushed[0];
        assert!(delta.starts_with("<environment_context>"), "{delta}");
        assert!(delta.contains(&format!("<cwd>{}</cwd>", current.cwd)));
        assert!(!delta.contains("<shell>"), "{delta}");
        assert!(!delta.contains("<current_date>"), "{delta}");
        assert!(!delta.contains("<timezone>"), "{delta}");
        assert_eq!(pushed[1], "turn");
        assert_eq!(
            session
                .reference_context_item
                .as_ref()
                .map(|item| &item.cwd),
            Some(&current.cwd)
        );
    }

    #[tokio::test]
    async fn shell_only_environment_change_emits_no_delta() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("ok".into())]);
        let current = crate::context::EnvironmentContext::from_tool_cx(&session.cx);
        session.reference_context_item = Some(crate::context::TurnContextItem {
            cwd: current.cwd.clone(),
            shell: Some("different-shell".into()),
            current_date: current.current_date.clone(),
            timezone: current.timezone.clone(),
        });

        run_user_turn(&mut session, "turn").await;

        let pushed = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(pushed, vec!["turn".to_string()]);
    }

    fn occurrences(haystack: &str, needle: &str) -> usize {
        haystack.match_indices(needle).count()
    }

    #[test]
    fn discovered_agents_move_to_context_before_environment() {
        let (mut session, shared) = mk_session(vec![]);
        let agents = "AGENTS_UNIQUE_RULE";
        session.user_instructions = Some(crate::context::UserInstructions {
            directory: "/repo".into(),
            text: agents.into(),
            loaded_paths: Vec::new(),
        });

        let system = compose_system(&session.system_sections(), &session.reg, false);
        assert!(
            !system.stable_text().unwrap_or("").contains(agents),
            "AGENTS text must not stay in system stable on the codex-shaped strategy"
        );

        session.push_user_text("hello");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.len(), 2);
        let context = &pushed[0];
        let user_idx = context.find("# AGENTS.md instructions").unwrap();
        let env_idx = context.find("<environment_context>").unwrap();
        assert!(user_idx < env_idx, "{context}");
        assert_eq!(
            occurrences(system.stable_text().unwrap_or(""), agents)
                + occurrences(context, agents)
                + occurrences(&pushed[1], agents),
            1
        );
    }

    #[test]
    fn no_agents_emits_only_environment_context_and_pinned_system() {
        let (mut session, shared) = mk_session(vec![]);

        let system = compose_system(&session.system_sections(), &session.reg, false);
        let stable = system.stable_text().expect("pinned tools stable block");
        assert!(stable.contains("Always-available tools"));
        assert!(!stable.contains("AGENTS_UNIQUE_RULE"));

        session.push_user_text("hello");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.len(), 2);
        assert!(pushed[0].starts_with("<environment_context>"));
        assert!(!pushed[0].contains("# AGENTS.md instructions"));
        assert_eq!(pushed[1], "hello");
    }

    #[test]
    fn explicit_system_override_stays_system_and_not_user_instructions() {
        let (mut session, shared) = mk_session(vec![]);
        let explicit = "EXPLICIT_SYSTEM_UNIQUE";
        session.explicit_system = Some(explicit.into());
        session.user_instructions = None;

        let system = compose_system(&session.system_sections(), &session.reg, false);
        assert!(system.stable_text().unwrap().contains(explicit));

        session.push_user_text("hello");

        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(
            occurrences(system.stable_text().unwrap_or(""), explicit)
                + occurrences(&pushed.join("\n"), explicit),
            1
        );
        assert!(pushed[0].starts_with("<environment_context>"));
        assert!(!pushed[0].contains("# AGENTS.md instructions"));
    }

    fn user_turns_after_initial_context(users: &[String]) -> &[String] {
        assert!(users[0].starts_with("<environment_context>"), "{users:?}");
        &users[1..]
    }

    // --- dispatch-context composition strategies (dispatch-prompt-slots.md §5/§7) ---

    fn test_dispatch_state(
        scope: Option<crate::context::dispatch::DispatchScope>,
    ) -> crate::context::dispatch::DispatchState {
        use crate::context::dispatch::*;
        let ctx = DispatchContext {
            v: 1,
            persona: Some("PERSONA_UNIQUE reviewer".into()),
            directives: vec![
                DispatchDirective {
                    id: "task_shape".into(),
                    cadence: DirectiveCadence::Standing,
                    needs_scope: false,
                    text: "STANDING_UNIQUE task-shape check".into(),
                },
                DispatchDirective {
                    id: "recall".into(),
                    cadence: DirectiveCadence::PerTurn,
                    needs_scope: false,
                    text: "PER_TURN_UNIQUE recall".into(),
                },
            ],
            scope,
            pins: Some("PINS_UNIQUE active arc".into()),
        };
        DispatchState::from_arg(DispatchContextArg::Provided(Box::new(ctx)), &Value::Null)
    }

    fn test_scope(task: &str) -> crate::context::dispatch::DispatchScope {
        crate::context::dispatch::DispatchScope {
            task: Some(task.into()),
            session: Some("sess-1".into()),
            ..Default::default()
        }
    }

    fn ordered<'a>(haystack: &'a str, needles: &[&str]) {
        let mut last = 0usize;
        for needle in needles {
            let idx = haystack
                .find(needle)
                .unwrap_or_else(|| panic!("{needle} missing from:\n{haystack}"));
            assert!(idx >= last, "{needle} out of order in:\n{haystack}");
            last = idx;
        }
    }

    #[test]
    fn codex_shaped_initial_context_orders_agents_scope_pins_env() {
        let (mut session, shared) = mk_session(vec![]);
        session.user_instructions = Some(crate::context::UserInstructions {
            directory: "/repo".into(),
            text: "AGENTS_UNIQUE_RULE".into(),
            loaded_paths: Vec::new(),
        });
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));

        // Persona + standing directives ride the stable system slot; per-turn
        // directives do NOT (they ride the volatile tail per request); memory/
        // scope/pins do NOT (contextual user lane).
        let system = compose_system(&session.system_sections(), &session.reg, false);
        let stable = system.stable_text().unwrap();
        ordered(stable, &["PERSONA_UNIQUE", "STANDING_UNIQUE"]);
        for absent in [
            "PER_TURN_UNIQUE",
            "AGENTS_UNIQUE_RULE",
            "<bbox_scope>",
            "<bbox_pins>",
            "<environment_context>",
        ] {
            assert!(!stable.contains(absent), "{absent} must not ride stable");
        }

        session.push_user_text("hello");
        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(pushed.len(), 2);
        // Turn-1 contextual user message ordering (codex order): AGENTS →
        // scope → pins → environment LAST.
        ordered(
            &pushed[0],
            &[
                "# AGENTS.md instructions",
                "<bbox_scope>",
                "task: task-1",
                "<bbox_pins>",
                "PINS_UNIQUE",
                "<environment_context>",
            ],
        );
        assert_eq!(pushed[1], "hello");
        // Baselines recorded for change/compaction re-emit.
        assert!(session.dispatch.emitted_scope.is_some());
        assert!(session.dispatch.emitted_pins.is_some());
    }

    #[test]
    fn vibe_shaped_folds_context_into_stable_and_keeps_user_lane_clean() {
        let (mut session, shared) = mk_session(vec![]);
        session.strategy = crate::context::dispatch::CompositionStrategy::VibeShaped;
        session.user_instructions = Some(crate::context::UserInstructions {
            directory: "/repo".into(),
            text: "AGENTS_UNIQUE_RULE".into(),
            loaded_paths: Vec::new(),
        });
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));

        // The leading-block ordering trades cache granularity for salience:
        // stable-first, per-resume-mutable sections (scope/pins) at the
        // suffix (design §5).
        let system = compose_system(&session.system_sections(), &session.reg, false);
        let stable = system.stable_text().unwrap();
        ordered(
            stable,
            &[
                "PERSONA_UNIQUE",
                "STANDING_UNIQUE",
                "AGENTS_UNIQUE_RULE",
                "Always-available tools",
                "<environment_context>",
                "<bbox_scope>",
                "<bbox_pins>",
            ],
        );
        assert!(!stable.contains("PER_TURN_UNIQUE"));

        // The initial-context emitter contributes NOTHING to the user lane:
        // the task is the only user message (the gap-00efeb12 fix).
        session.push_user_text("one-line task");
        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(*pushed, vec!["one-line task".to_string()]);
    }

    #[test]
    fn vibe_shaped_post_compaction_emits_nothing_user_lane() {
        let (mut session, shared) = mk_session(vec![]);
        session.strategy = crate::context::dispatch::CompositionStrategy::VibeShaped;
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        session.push_user_text("turn one");
        // Compaction resets the baseline; the vibe-shaped strategy still
        // emits nothing — the leading system block is not part of the
        // compacted buffer.
        session.reference_context_item = None;
        session.prepare_context_for_user_turn();
        let pushed = shared.pushed_users.lock().unwrap();
        assert_eq!(*pushed, vec!["turn one".to_string()]);
        assert!(
            session.reference_context_item.is_some(),
            "baseline advanced"
        );
    }

    #[test]
    fn codex_shaped_scope_change_re_emits_fragment_once() {
        let (mut session, shared) = mk_session(vec![]);
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        session.push_user_text("turn one");
        assert_eq!(shared.pushed_users.lock().unwrap().len(), 2);

        // Same scope on the next turn ⇒ nothing re-emitted.
        session.prepare_context_for_user_turn();
        assert_eq!(shared.pushed_users.lock().unwrap().len(), 2);

        // A resume re-passing a CHANGED scope ⇒ one short fragment, baseline
        // advanced. Pins unchanged ⇒ not re-emitted.
        session.dispatch.context.as_mut().unwrap().scope = Some(test_scope("task-2"));
        session.prepare_context_for_user_turn();
        {
            let pushed = shared.pushed_users.lock().unwrap();
            assert_eq!(pushed.len(), 3);
            assert!(pushed[2].starts_with("<bbox_scope>"), "{}", pushed[2]);
            assert!(pushed[2].contains("task: task-2"));
            assert!(!pushed[2].contains("<bbox_pins>"));
        }

        // And it converges: same scope again ⇒ silent.
        session.prepare_context_for_user_turn();
        assert_eq!(shared.pushed_users.lock().unwrap().len(), 3);
    }

    #[test]
    fn codex_shaped_no_scope_emits_nothing_and_keeps_baseline() {
        // Restored session (scope never restored): no scope ⇒ no fragment,
        // and the persisted baseline survives for future delta comparison.
        let (mut session, shared) = mk_session(vec![]);
        session.dispatch = test_dispatch_state(None);
        session.dispatch.emitted_scope = Some("<bbox_scope>\ntask: old\n</bbox_scope>".into());
        session.push_user_text("follow-up");
        let pushed = shared.pushed_users.lock().unwrap();
        // Initial context = pins + environment only (no scope, no AGENTS).
        assert_eq!(pushed.len(), 2);
        assert!(!pushed[0].contains("<bbox_scope>"));
        assert!(pushed[0].contains("<bbox_pins>"));
        assert_eq!(
            session.dispatch.emitted_scope.as_deref(),
            Some("<bbox_scope>\ntask: old\n</bbox_scope>"),
            "baseline must survive a scope-less run"
        );
    }

    #[test]
    fn codex_shaped_post_compaction_re_emits_current_context() {
        let (mut session, shared) = mk_session(vec![]);
        session.user_instructions = Some(crate::context::UserInstructions {
            directory: "/repo".into(),
            text: "AGENTS_UNIQUE_RULE".into(),
            loaded_paths: Vec::new(),
        });
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        session.push_user_text("turn one");

        // Simulate a resume that changed the scope, then compaction resetting
        // the reference item (agent_loop compaction paths set it to None):
        // the deterministic re-emit renders the CURRENT in-memory context.
        session.dispatch.context.as_mut().unwrap().scope = Some(test_scope("task-9"));
        session.reference_context_item = None;
        session.prepare_context_for_user_turn();

        let pushed = shared.pushed_users.lock().unwrap();
        let re_emitted = pushed.last().unwrap();
        ordered(
            re_emitted,
            &[
                "# AGENTS.md instructions",
                "<bbox_scope>",
                "task: task-9",
                "<bbox_pins>",
                "<environment_context>",
            ],
        );
        assert_eq!(
            session.dispatch.emitted_scope.as_deref(),
            session.dispatch.scope_render().as_deref(),
            "post-compaction re-emit must update the baseline"
        );
    }

    #[test]
    fn suppressed_defaults_with_dispatch_context_keeps_persona_and_directives() {
        // `--system-prompt ""` clears explicit_system AND disables AGENTS
        // discovery, but a dispatch context still lands persona + directives
        // in stable (design §8): base + persona + directives, no AGENTS.
        let (mut session, _shared) = mk_session(vec![]);
        session.explicit_system = None;
        session.user_instructions = None;
        session.dispatch = test_dispatch_state(None);
        let system = compose_system(&session.system_sections(), &session.reg, false);
        let stable = system.stable_text().unwrap();
        assert!(stable.contains("PERSONA_UNIQUE"));
        assert!(stable.contains("STANDING_UNIQUE"));
        assert!(!stable.contains("# AGENTS.md instructions"));
    }

    #[tokio::test]
    async fn per_turn_directives_ride_volatile_after_nudge() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("done".into())]);
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        session.tail_nudge = Some("NUDGE_UNIQUE".into());
        run_user_turn(&mut session, "go").await;

        let systems = shared.seen_systems.lock().unwrap();
        let volatile = systems
            .last()
            .and_then(|s| s.volatile_text())
            .expect("volatile tail present");
        // Per-turn directives share the volatile lane with the existing
        // channels, AFTER them (design §8).
        ordered(volatile, &["NUDGE_UNIQUE", "PER_TURN_UNIQUE"]);
        let stable = systems.last().and_then(|s| s.stable_text()).unwrap();
        assert!(!stable.contains("PER_TURN_UNIQUE"));
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
        assert_eq!(
            user_turns_after_initial_context(&users),
            &["alpha".to_string(), "beta".to_string()]
        );
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
        assert_eq!(
            user_turns_after_initial_context(&users),
            &["alpha".to_string(), "beta".to_string()]
        );
        assert_eq!(shared.completed.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn responses_end_turn_false_without_tools_samples_again() {
        let (mut session, shared) = mk_session(vec![
            MockTurn::TextWithEndTurn("partial".into(), Some(false)),
            MockTurn::TextWithEndTurn("done".into(), Some(true)),
        ]);

        run_user_turn(&mut session, "continue please").await;

        assert_eq!(shared.completed.load(Ordering::SeqCst), 2);
        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(
            user_turns_after_initial_context(&users),
            &["continue please".to_string()]
        );
    }

    #[tokio::test]
    async fn done_without_end_turn_breaks_after_one_model_call() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("done".into())]);

        run_user_turn(&mut session, "stop normally").await;

        assert_eq!(shared.completed.load(Ordering::SeqCst), 1);
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
        assert_eq!(
            user_turns_after_initial_context(&users),
            &["alpha".to_string(), "beta".to_string()]
        );
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

    #[test]
    fn turn_end_diagnostics_flags_running_shell_session() {
        let (session, _shared) = mk_session(vec![]);

        let trace = tool_result_trace(
            &transport::ToolCall {
                id: "tc-1".into(),
                name: "shell_run".into(),
                args: json!({}),
            },
            &transport::ToolResult {
                id: "tc-1".into(),
                content: json!({
                    "session_id": "sh-1",
                    "running": true,
                })
                .to_string(),
                is_error: false,
            },
        );

        // A still-running shell session (last tool result `running:true`) flags
        // outstanding async work independently of the empty-output heuristic.
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
        assert_eq!(diag["last_tool_results"][0]["running"], true);
        assert_eq!(diag["produced_text"], true);
        assert_eq!(diag["empty_output_stop"], false);
        assert_eq!(diag["suspicious"], true);
        assert!(
            diag["suspicion_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "last_tool_running"),
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
        assert_eq!(diag["last_turn_text_len"], 0);
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
        session.append_edit_diagnostics(&mut content).await;

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
        session.append_edit_diagnostics(&mut content).await;
        assert_eq!(content, "{\"ok\":true}", "no edits -> no rider appended");
    }
}

#[cfg(test)]
mod shell_env_tests {
    use super::*;

    #[test]
    fn load_shell_env_precedence_explicit_then_cli() {
        let explicit = BTreeMap::from([("A".to_string(), "explicit".to_string())]);
        let via_explicit = load_shell_env(Some(explicit), Some(r#"{"A":"cli"}"#)).unwrap();
        assert_eq!(via_explicit.get("A").map(String::as_str), Some("explicit"));

        let via_cli = load_shell_env(None, Some(r#"{"A":"cli"}"#)).unwrap();
        assert_eq!(via_cli.get("A").map(String::as_str), Some("cli"));

        assert!(load_shell_env(None, Some("not json")).is_err());
    }
}

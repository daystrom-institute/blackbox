//! The transport-agnostic tool-calling loop.
//!
//! Two entry modes, both built on one [`Session`]:
//!
//! - **One-shot** (default): resolve a single prompt, run one user turn to
//!   completion, emit `result`, persist, exit.
//! - **Bidirectional** (`--input-format stream-json`): keep a persistent session
//!   alive, reading successive user-turn messages and `control_request`s
//!   (interrupt, set_model, …) from stdin as NDJSON, and `/compact` as an
//!   in-stream slash command. This is the fleet-cockpit control plane
//!   (design/fleet-tui/fleet-tui.md §2). Wire shapes follow the Claude Agent
//!   SDK control protocol (hyperclaude SDK_PROTOCOL.md / NDJSON_FORMAT.md).
//! - **Daemon child** (`--input-format stream-json --exit-when-idle`): wait for
//!   the daemon's first stdin turn, drain that turn and buffered controls, then
//!   persist and exit. blackboxd uses one such child per dispatch.
//!
//! The transport handles all wire differences; the loop and the stdout envelope
//! are identical across providers.

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
use std::sync::atomic::{AtomicU64, Ordering};
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
that conform to its input schema. Submit it alone in its response. That call ends \
the session; do not make further tool calls after it.";

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

fn compile_output_schema(schema: &Value) -> Result<jsonschema::JSONSchema> {
    let mut options = jsonschema::JSONSchema::options();
    if schema.get("$schema").is_none() {
        options.with_draft(jsonschema::Draft::Draft202012);
    }
    options
        .compile(schema)
        .map_err(|error| anyhow::anyhow!("invalid output schema: {error}"))
}

fn final_result_rejection(schema: Option<&Value>, calls: &[transport::ToolCall]) -> Option<String> {
    let schema = schema?;
    let terminal = calls.iter().find(|call| call.name == FINAL_RESULT_TOOL)?;
    if calls.len() != 1 {
        return Some("final_result must be the only call in its response. No calls in this batch were executed; complete other work before submitting the final result.".into());
    }
    let compiled = match compile_output_schema(schema) {
        Ok(compiled) => compiled,
        Err(error) => return Some(error.to_string()),
    };
    if let Err(errors) = compiled.validate(&terminal.args) {
        let details = errors
            .take(8)
            .map(|error| format!("{}: {}", error.instance_path, error))
            .collect::<Vec<_>>()
            .join("; ");
        return Some(format!(
            "final_result did not match the output schema: {details}. Correct the result; nothing was executed."
        ));
    }
    None
}

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
    run_with_event_callback_and_input_mcp(cli, input_rx, callback, None, None, None).await
}

pub async fn run_with_event_callback_and_input_mcp(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: EventCallback,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, Value>>,
    shell_env: Option<BTreeMap<String, String>>,
) -> Result<()> {
    run_controlled_session(
        cli,
        input_rx,
        Some(callback),
        mcp_config,
        additional_context,
        shell_env,
    )
    .await
}

async fn run_with_emitter(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
) -> Result<()> {
    if cli.input_format.as_deref() == Some("stream-json") {
        return run_session(cli, callback, mcp_config, None).await;
    }

    let prompt = resolve_prompt(&cli)?;
    let mut session = Session::build(&cli, callback, mcp_config, None, None).await?;
    session.emitter.system_init();
    let mut pending = std::mem::take(&mut session.pending_user_inputs);
    pending.push_back(prompt);
    // A resumed redirect precedes the newly supplied prompt. Every completed
    // turn checkpoints the remaining queue before proceeding to the next input.
    while let Some(prompt) = pending.pop_front() {
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        let turn_result = if prompt.trim() == "/compact" {
            session.drain_cancelled_work().await;
            let result = session.compact_manual().await;
            if let Err(error) = &result {
                session
                    .emitter
                    .result_error(&format!("manual /compact failed: {error:#}"), session.turns);
            }
            result
        } else {
            session
                .user_turn(&prompt, cancel_rx, Arc::new(StdMutex::new(VecDeque::new())))
                .await
        };
        session.pending_user_inputs = pending.clone();
        // Keep completed exchanges and unconsumed queued inputs even when the
        // current turn failed, while retaining one-shot error exit semantics.
        let persist_result = session.persist().await;
        match (turn_result, persist_result) {
            (Err(error), Err(persist_error)) => {
                return Err(error.context(format!(
                    "session persistence also failed: {persist_error:#}"
                )));
            }
            (Err(error), Ok(())) => return Err(error),
            (Ok(()), persisted) => persisted?,
        }
    }
    Ok(())
}

/// Builds an `Emitter` for `session_id`, wiring in the sidecar event log and
/// the session's shared seq counter. Every emitter built for the SAME
/// session (the loop's own emitter, the stdin-reader's replay emitter, the
/// control-response emitter, the `report` tool's emitter) must be given the
/// SAME `seq_counter` `Arc` (see `Emitter::with_seq_counter`), so they hand
/// out one strictly monotonically increasing `seq` stream instead of
/// colliding.
fn make_emitter(
    session_id: String,
    callback: Option<EventCallback>,
    event_log: Option<Arc<EventLog>>,
    seq_counter: Arc<AtomicU64>,
) -> Emitter {
    let emitter = match callback {
        Some(callback) => Emitter::with_callback(session_id, callback),
        None => Emitter::new(session_id),
    };
    let emitter = match event_log {
        Some(log) => emitter.with_event_log(log),
        None => emitter,
    };
    emitter.with_seq_counter(seq_counter)
}

/// Bidirectional persistent session driven over stdin NDJSON.
async fn run_session(
    cli: Cli,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, Value>>,
) -> Result<()> {
    let replay = cli.replay_user_messages;
    let exit_when_idle = cli.exit_when_idle;
    let mut session =
        Session::build(&cli, callback.clone(), mcp_config, additional_context, None).await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();

    // The stdin reader runs as its own task so control messages (interrupt)
    // arrive while a turn is in flight. It owns a clone of the emitter purely to
    // honour `--replay-user-messages`.
    let mut input_rx = spawn_stdin_reader(
        replay,
        make_emitter(
            sid.clone(),
            callback.clone(),
            Some(session.event_log()),
            session.seq_counter(),
        ),
    );
    // A separate emitter for control responses emitted *during* a turn, when the
    // session's own emitter is borrowed by the running turn.
    let ctrl_emitter = make_emitter(
        sid,
        callback,
        Some(session.event_log()),
        session.seq_counter(),
    );

    // Steers that arrived mid-turn wait here for the next turn boundary.
    let mut pending = std::mem::take(&mut session.pending_user_inputs);
    // An initial `-p` prompt (if any) is the first user turn.
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }

    if exit_when_idle {
        await_first_controlled_input(&mut session, &mut input_rx, &ctrl_emitter, &mut pending)
            .await?;
        session_loop_until_idle(&mut session, input_rx, &ctrl_emitter, pending).await?;
    } else {
        session_loop(&mut session, input_rx, &ctrl_emitter, pending).await?;
    }
    Ok(())
}

async fn run_controlled_session(
    cli: Cli,
    input_rx: SessionInputReceiver,
    callback: Option<EventCallback>,
    mcp_config: Option<mcp::McpConfig>,
    additional_context: Option<BTreeMap<String, Value>>,
    shell_env: Option<BTreeMap<String, String>>,
) -> Result<()> {
    let mut session = Session::build(
        &cli,
        callback.clone(),
        mcp_config,
        additional_context,
        shell_env,
    )
    .await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();
    let ctrl_emitter = make_emitter(
        sid,
        callback,
        Some(session.event_log()),
        session.seq_counter(),
    );

    let mut pending = std::mem::take(&mut session.pending_user_inputs);
    if let Some(p) = cli.prompt.clone() {
        pending.push_back(p);
    }
    let input_rx = map_session_input(input_rx);

    session_loop_until_idle(&mut session, input_rx, &ctrl_emitter, pending).await?;
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

#[derive(Debug)]
enum SessionControl {
    Interrupt { redirect: Option<String> },
    SetModel(String),
}

fn restore_pending_user_inputs(side: &Value) -> Result<VecDeque<String>> {
    let Some(value) = side.get("pending_user_inputs") else {
        return Ok(VecDeque::new());
    };
    value
        .as_array()
        .context("persisted pending_user_inputs must be an array")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .context("persisted pending input must be a string")
        })
        .collect()
}

fn parse_control(subtype: &str, raw: &Value) -> Result<SessionControl> {
    anyhow::ensure!(raw.is_object(), "control payload must be an object");
    let field = |name: &str| {
        raw.get(name)
            .or_else(|| raw.get("request").and_then(|request| request.get(name)))
    };
    match subtype {
        "interrupt" => {
            let redirect = field("prompt")
                .map(|value| {
                    value
                        .as_str()
                        .filter(|text| !text.trim().is_empty())
                        .map(str::to_owned)
                        .context("interrupt prompt must be a nonempty string")
                })
                .transpose()?;
            Ok(SessionControl::Interrupt { redirect })
        }
        "set_model" => {
            let model = field("model")
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .context("set_model requires a nonempty model string")?;
            Ok(SessionControl::SetModel(model.to_owned()))
        }
        _ => anyhow::bail!("unsupported control: {subtype}"),
    }
}

struct PendingControl {
    command: SessionControl,
    request_id: Option<String>,
}

fn receive_control(
    subtype: &str,
    raw: &Value,
    request_id: Option<String>,
    emitter: &Emitter,
) -> Option<PendingControl> {
    match parse_control(subtype, raw) {
        Ok(command) => Some(PendingControl {
            command,
            request_id,
        }),
        Err(error) => {
            emitter.control_response_error(request_id.as_deref(), &error.to_string());
            None
        }
    }
}

async fn apply_pending_control(
    session: &mut Session,
    control: PendingControl,
    emitter: &Emitter,
    pending: &mut VecDeque<String>,
) -> Result<()> {
    match control.command {
        SessionControl::Interrupt { redirect } => {
            session.drain_cancelled_work().await;
            if let Some(prompt) = redirect {
                pending.push_front(prompt);
            }
        }
        SessionControl::SetModel(model) => {
            if let Err(error) = session.apply_control(&model).await {
                // Draining work can append durable outcomes even when the
                // compaction fails. Checkpoint them under the unchanged model.
                session.pending_user_inputs = pending.clone();
                session.persist().await?;
                emitter.control_response_error(
                    control.request_id.as_deref(),
                    &format!("model change rejected: {error:#}"),
                );
                return Ok(());
            }
        }
    }
    session.pending_user_inputs = pending.clone();
    match session.persist().await {
        Ok(()) => emitter.control_response_success(control.request_id.as_deref()),
        Err(error) => {
            emitter.control_response_error(
                control.request_id.as_deref(),
                &format!("control applied but persistence failed: {error:#}"),
            );
            session.drain_cancelled_work().await;
            return Err(error);
        }
    }
    Ok(())
}

/// Both entry modes use the same turn/control boundary and durable persistence.
async fn session_loop(
    session: &mut Session,
    mut input_rx: mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    mut pending: VecDeque<String>,
) -> Result<()> {
    loop {
        let prompt = match pending.pop_front() {
            Some(prompt) => prompt,
            None => match input_rx.recv().await {
                Some(Input::User(prompt)) => prompt,
                Some(Input::Control {
                    subtype,
                    req_id,
                    raw,
                }) => {
                    if let Some(control) = receive_control(&subtype, &raw, req_id, ctrl_emitter) {
                        apply_pending_control(session, control, ctrl_emitter, &mut pending).await?;
                    }
                    continue;
                }
                None => break,
            },
        };
        run_prompt_with_controls(session, &mut input_rx, ctrl_emitter, &mut pending, prompt)
            .await?;
    }
    session.drain_cancelled_work().await;
    session.persist().await
}

async fn session_loop_until_idle(
    session: &mut Session,
    mut input_rx: mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    mut pending: VecDeque<String>,
) -> Result<()> {
    loop {
        let prompt = match pending.pop_front() {
            Some(prompt) => prompt,
            None => match input_rx.try_recv() {
                Ok(Input::User(prompt)) => prompt,
                Ok(Input::Control {
                    subtype,
                    req_id,
                    raw,
                }) => {
                    if let Some(control) = receive_control(&subtype, &raw, req_id, ctrl_emitter) {
                        apply_pending_control(session, control, ctrl_emitter, &mut pending).await?;
                    }
                    continue;
                }
                Err(_) => break,
            },
        };
        run_prompt_with_controls(session, &mut input_rx, ctrl_emitter, &mut pending, prompt)
            .await?;
    }
    session.drain_cancelled_work().await;
    session.persist().await
}

/// Child startup waits for the first message before switching to idle draining.
async fn await_first_controlled_input(
    session: &mut Session,
    input_rx: &mut mpsc::UnboundedReceiver<Input>,
    ctrl_emitter: &Emitter,
    pending: &mut VecDeque<String>,
) -> Result<()> {
    while pending.is_empty() {
        match input_rx.recv().await {
            Some(Input::User(prompt)) => pending.push_back(prompt),
            Some(Input::Control {
                subtype,
                req_id,
                raw,
            }) => {
                if let Some(control) = receive_control(&subtype, &raw, req_id, ctrl_emitter) {
                    apply_pending_control(session, control, ctrl_emitter, pending).await?;
                }
            }
            None => break,
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
    let compact = prompt.trim() == "/compact";
    if compact {
        // Replacement history must follow the actual outcomes of active work.
        session.drain_cancelled_work().await;
    }
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let mut deferred = Vec::new();
    let mut stdin_closed = false;
    let mid_turn_user_inputs = Arc::new(StdMutex::new(VecDeque::new()));
    let turn_result = {
        let operation = async {
            if compact {
                session.compact_manual().await
            } else {
                session
                    .user_turn(&prompt, cancel_rx, mid_turn_user_inputs.clone())
                    .await
            }
        };
        tokio::pin!(operation);
        loop {
            tokio::select! {
                biased;
                result = &mut operation => break result,
                maybe = input_rx.recv(), if !stdin_closed => match maybe {
                    Some(Input::Control { subtype, req_id, raw }) => {
                        if let Some(control) = receive_control(&subtype, &raw, req_id, ctrl_emitter) {
                            if matches!(&control.command, SessionControl::Interrupt { .. }) {
                                let _ = cancel_tx.send(true);
                            }
                            deferred.push(control);
                        }
                    }
                    Some(Input::User(prompt)) if compact || prompt.trim() == "/compact" => pending.push_back(prompt),
                    Some(Input::User(prompt)) => {
                        if let Ok(mut inputs) = mid_turn_user_inputs.lock() { inputs.push_back(prompt); }
                    }
                    None => {
                        // EOF ends input admission; already accepted turns finish.
                        // Explicit interrupt requests own cancellation.
                        stdin_closed = true;
                    }
                }
            }
        }
    };
    if let Err(error) = turn_result {
        if compact {
            session
                .emitter
                .result_error(&format!("manual /compact failed: {error:#}"), session.turns);
        } else {
            tracing::error!("turn failed: {error:#}");
        }
    }
    if let Ok(mut inputs) = mid_turn_user_inputs.lock() {
        while let Some(prompt) = inputs.pop_back() {
            pending.push_front(prompt);
        }
    }
    // Preserve queued redirects and commands as well as completed operations.
    session.pending_user_inputs = pending.clone();
    // Preserve the completed operation before acknowledging any later controls.
    if let Err(error) = session.persist().await {
        for control in deferred {
            let status = if matches!(&control.command, SessionControl::Interrupt { .. }) {
                "interrupt completed but its redirect was not queued"
            } else {
                "control was not applied"
            };
            ctrl_emitter.control_response_error(
                control.request_id.as_deref(),
                &format!("{status}: session persistence failed: {error:#}"),
            );
        }
        session.drain_cancelled_work().await;
        return Err(error);
    }
    let mut deferred = deferred.into_iter();
    while let Some(control) = deferred.next() {
        if let Err(error) = apply_pending_control(session, control, ctrl_emitter, pending).await {
            for remaining in deferred {
                let status = if matches!(&remaining.command, SessionControl::Interrupt { .. }) {
                    "interrupt completed but its redirect was not queued"
                } else {
                    "control was not applied"
                };
                ctrl_emitter.control_response_error(
                    remaining.request_id.as_deref(),
                    &format!("{status}: preceding control persistence failed: {error:#}"),
                );
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Persistent per-dispatch state, shared by both entry modes.
struct Session {
    tx: Box<dyn Transport>,
    reg: Registry,
    /// Resolved code-mode for this session. Session-intrinsic (like `model`):
    /// persisted in the session file and restored on resume so the surface
    /// shape stays consistent with any `exec` cells already in the transcript.
    code_mode: crate::code_mode::CodeMode,
    code_mode_session: Option<crate::code_mode::CodeModeToolSession>,
    remote_outcome_sources: Vec<Arc<dyn Tool>>,
    retain_background_work: bool,
    cx: ToolCx,
    reference_context_item: Option<crate::context::TurnContextItem>,
    hooks: HookEngine,
    scoped_project_docs: Arc<crate::project_doc::ScopedProjectDocs>,
    resume_runtime_reset: bool,
    emitter: Emitter,
    base_opts: TurnOpts,
    /// Explicit caller-supplied system text.
    explicit_system: Option<String>,
    /// Captured full instruction section for strategies that rebuild memory in
    /// system. The typed ledger owns versions and delivery receipts.
    instruction_system: Option<String>,
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
    /// The active model's context window, when the compaction table knows it.
    /// Published on the per-step `context_pressure` event so consumers get the
    /// utilization denominator from the table's owner instead of duplicating
    /// it. `None` for a model the table does not recognize; consumers then
    /// report occupancy with no utilization rather than guessing.
    context_window: Option<u64>,
    /// Ordinary tool-result output limit in bytes (0 disables the limit).
    tool_result_cap: usize,
    store: SessionStore,
    /// Sidecar append-only timestamped event log (`event_log.rs`). The
    /// emitters tee every protocol event into it; the loop additionally logs
    /// user turns and compaction milestones. Best-effort — never fails a turn.
    event_log: Arc<EventLog>,
    /// Per-session monotonic event `seq` counter (`emit.rs`), shared by every
    /// emitter built for this session (`make_emitter`). Seeded at `build()`
    /// from `max(snapshot.last_event_seq, EventLog::max_seq_in_log(...))` so
    /// a resumed session continues the sequence rather than restarting at 0.
    seq_counter: Arc<AtomicU64>,
    prior_side: Value,
    pending_user_inputs: VecDeque<String>,
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
    /// Retained response output and locally appended context since the last
    /// measured request. Persisted atomically with the native history and its
    /// request overhead so resume never treats a populated transcript as empty.
    pending_input_estimate: u64,
    last_request_overhead_tokens: u64,
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
/// session resolver so library embedders can provide a task-local override.
/// Daemon-launched workers receive the per-dispatch value in their own process
/// environment; a directly launched binary reads the operator's shell env.
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
        additional_context: Option<BTreeMap<String, Value>>,
        shell_env: Option<BTreeMap<String, String>>,
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
        //   absent    ⇒ discover typed filesystem instructions, delivered at
        //               the model boundary according to the provider strategy.
        // Per-session working directory: explicit `--cwd` (the daemon's
        // dispatch cwd, passed instead of mutating the process cwd) or the
        // process cwd for the standalone binary. All file/shell tools and
        // project-doc discovery resolve against this root, so concurrent
        // child sessions never collide (harness-process-boundary.md §2).
        let root = match cli.cwd.as_deref() {
            Some(c) => std::fs::canonicalize(c).unwrap_or_else(|_| std::path::PathBuf::from(c)),
            None => std::env::current_dir().context("cwd")?,
        };

        let explicit_system = cli
            .system_prompt
            .as_deref()
            .filter(|text| !text.is_empty())
            .map(str::to_owned);

        let kind = TransportKind::from_env();
        let mut tx = transport::build_transport(kind).await?;

        let store = SessionStore::open(cli.session_id.as_deref(), cli.resume.as_deref())?;
        // Sidecar append-only event log next to the snapshot — the durable
        // timestamped record of this session (event_log.rs).
        let event_log = Arc::new(EventLog::for_session(&store.id));
        // Seed the live seq counter (emit.rs) from the persisted snapshot,
        // reconciled against the event log's own tail. The snapshot alone can
        // be stale: it only persists at turn boundaries, so a crash between
        // persists can leave it behind lines the log already durably recorded
        // (append-only, one write_all per line). Taking the max means a
        // resumed session never reuses a `seq` for a DIFFERENT event, which
        // would poison a fleetd replay cursor
        // (design/daemon-runtime/locality-first-decomposition.md slice 5).
        // Fresh sessions: both sides are 0, so the first emitted event is
        // seq 1.
        let restored_last_event_seq = store.restored.as_ref().map_or(0, |r| r.last_event_seq);
        let log_tail_seq = EventLog::max_seq_in_log(event_log.path());
        let seq_counter = Arc::new(AtomicU64::new(restored_last_event_seq.max(log_tail_seq)));
        let scoped_project_docs = Arc::new(crate::project_doc::ScopedProjectDocs::for_session(
            root.clone(),
            cli.system_prompt.as_deref(),
        ));
        // Hand the transport the stable session id, so it can populate the
        // codex-style `session-id` header + `prompt_cache_key` (vs a random
        // per-request id).
        tx.set_session_id(store.id.clone());
        let restored_model = store.restored.as_ref().and_then(|r| r.model.clone());
        let restored_code_mode = store.restored.as_ref().and_then(|r| r.code_mode.clone());
        let restored_service_tier = store.restored.as_ref().and_then(|r| r.service_tier.clone());
        let restored_effort = store.restored.as_ref().and_then(|r| r.effort.clone());
        // Loop-level side cells restored from a prior turn. Each cell
        // deserializes its own slot tolerantly (absent/garbage → empty).
        let prior_side = store
            .restored
            .as_ref()
            .map(|r| r.side.clone())
            .unwrap_or(Value::Null);
        if let Some(paths) = prior_side.get("instruction_observed_paths") {
            scoped_project_docs.restore_observed_paths(
                serde_json::from_value(paths.clone())
                    .context("invalid persisted instruction scopes")?,
            );
        }
        if let Some(documents) = prior_side.get("instruction_documents") {
            let documents = serde_json::from_value(documents.clone())
                .context("invalid persisted instruction documents")?;
            scoped_project_docs
                .restore_documents(documents)
                .await
                .map_err(anyhow::Error::msg)?;
        }
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
        let restored_budget =
            crate::context::budget::BudgetCheckpoint::restore(&prior_side["context_budget"]);
        let dispatch_arg =
            crate::context::dispatch::resolve_dispatch_context_arg(cli.dispatch_context.as_deref())
                .map_err(anyhow::Error::msg)
                .context("--dispatch-context")?;
        let pending_user_inputs = restore_pending_user_inputs(&prior_side)?;
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
            .or(restored_model.clone())
            .or_else(|| transport::session_var("ANTHROPIC_MODEL"))
            .or_else(|| std::env::var("BRO_HARNESS_MODEL").ok())
            .context(
                "no --model, no resumed session model, and no ANTHROPIC_MODEL/BRO_HARNESS_MODEL",
            )?;
        let tool_arg_defaults =
            load_tool_arg_defaults(additional_context, cli.additional_context.as_deref())?;
        let shell_env = load_shell_env(shell_env, cli.shell_env.as_deref())?;
        let child_env = if cli.daemon_worker {
            bro_tools::ChildEnvironment::new(
                std::env::var("BRO_HARNESS_SPAWN_SCRUB")
                    .unwrap_or_default()
                    .split(',')
                    .map(str::to_string),
            )
        } else {
            bro_tools::ChildEnvironment::default()
        };

        let edits = Arc::new(std::sync::Mutex::new(bro_tools::EditSink::default()));
        let tool_result_cap = crate::bound::cap_bytes();
        let cx = ToolCx {
            root: root.clone(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            edits: edits.clone(),
            session_env: Arc::new(transport::session_env_snapshot()),
            output_budget: tool_result_cap,
            child_env: Arc::new(child_env),
            cancellation: Default::default(),
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: Some(scoped_project_docs.clone()),
            tool_arg_defaults: Arc::new(tool_arg_defaults),
            shell_env: Arc::new(shell_env),
        };
        let lsp_config = crate::bindings::lsp_config_for_context(&cx);
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
            seq_counter.clone(),
        ))));
        let (mcp_loaded, tool_placement) = match injected_mcp {
            Some(config) => {
                let tools = mcp::load_mcp_tools_from_config_with_capability_aliases(
                    &config,
                    &tool_filter,
                    cli.capability_mcp_server.as_deref(),
                )
                .await?;
                (tools, config.tool_placement)
            }
            None => {
                let tools = mcp::load_mcp_tools_with_capability_aliases(
                    cli.mcp_config.as_deref(),
                    &tool_filter,
                    cli.capability_mcp_server.as_deref(),
                )
                .await?;
                let placement = mcp::parse_tool_placement(cli.mcp_config.as_deref())?;
                (tools, placement)
            }
        };
        make_emitter(
            store.id.clone(),
            callback.clone(),
            Some(event_log.clone()),
            seq_counter.clone(),
        )
        .mcp_readiness(&mcp_loaded.readiness);
        let remote_outcome_sources = mcp_loaded.tools.clone();
        let mcp_tools = crate::locality::install_project_mutation_routes(
            mcp_loaded.tools,
            &cx,
            cli.capability_mcp_server.as_deref(),
        )
        .await?;
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
        // Code-mode (exec/wait) supersedes NARF as the authorial surface. The
        // callable set mirrors the flat surface: filtered builtins plus all MCP
        // (including daemon capability aliases), with a ToolCapability seam
        // over that same set
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

        let dispatch_gate = Arc::new(tokio::sync::RwLock::new(()));
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
        let mut code_mode_session = None;
        if code_mode.enables_code_surface() {
            // Domain bindings (code-mode-cell-dsl.md §5): cell-only constructs
            // projected as namespace globals (`code.*`). They join the callable
            // set + seam — the surface ToolFilter still gates them by canonical
            // name — but never the flat wire registry: a binding exists only
            // inside cells.
            cm_callable.extend(
                crate::bindings::BindingToolSession::with_lsp_config(lsp_config.clone())
                    .tools()
                    .into_iter()
                    .filter(|t| tool_filter.permits(t.name())),
            );
            cm_callable = bro_tools::prune_tool_dependencies(cm_callable);
            let cm_seam: Arc<dyn bro_capabilities::ToolCapability> =
                Arc::new(crate::capabilities::HostTools::with_dispatch_gate(
                    cm_callable.clone(),
                    cx.clone(),
                    dispatch_gate.clone(),
                ));
            let cm_session = crate::code_mode::CodeModeToolSession::new(
                &cm_callable,
                cm_seam,
                code_mode,
                &crate::bindings::namespace_descriptions(),
            );
            builtins.extend(cm_session.tools());
            code_mode_session = Some(cm_session);
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
            .map(serde_json::from_str::<Value>)
            .transpose()
            .context("invalid output schema JSON")?
            .or_else(|| {
                prior_side
                    .get("output_schema")
                    .filter(|value| !value.is_null())
                    .cloned()
            });
        if let Some(ref schema) = output_schema {
            compile_output_schema(schema)?;
            builtins.push(Arc::new(FinalResultTool::new(schema.clone())));
            pin.also_pin(FINAL_RESULT_TOOL);
        }

        let mut reg = Registry::with_options(
            builtins,
            mcp_out_box,
            &pin,
            &tool_filter,
            code_mode.defers_builtins(),
        )?;
        reg.set_dispatch_gate(dispatch_gate);
        if restored_snapshot {
            // Receipts are independent evidence, even when an explicit saved
            // activation list is empty. Neither source can erase the other.
            let path = event_log.path().to_path_buf();
            let snapshot = store.restored.as_ref().unwrap().snapshot.clone();
            let receipts = tokio::task::spawn_blocking(move || {
                let mut names = std::collections::BTreeSet::new();
                for source in [
                    EventLog::tool_search_activations(&path),
                    EventLog::snapshot_tool_search_activations(&snapshot),
                ] {
                    names.extend(
                        source
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
                json!(names)
            })
            .await
            .context("read resume tool activation receipts")?;
            reg.restore_resume_activations(prior_side.get("tool_activations"), &receipts);
        }
        validate_tool_arg_defaults(&cx.tool_arg_defaults, &reg);

        let base_opts = TurnOpts {
            base_instructions: Some(transport::base_instructions_for_capabilities(
                &reg.wire_specs(),
            )),
            model,
            max_tokens,
            system: SystemPrompt::default(),
            effort: cli.effort.clone().or(restored_effort),
            web_search,
            service_tier: cli
                .service_tier
                .clone()
                .or(restored_service_tier)
                .or_else(|| std::env::var("BRO_HARNESS_SERVICE_TIER").ok()),
        };

        let emitter = make_emitter(
            store.id.clone(),
            callback,
            Some(event_log.clone()),
            seq_counter.clone(),
        );
        if let Err(error) = reg.validate_resume_tool_schemas() {
            emitter.result_error(&format!("{error:#}"), 0);
            let log = event_log.clone();
            let _ = tokio::task::spawn_blocking(move || log.flush_blocking()).await;
            return Err(error);
        }
        let compaction = crate::compaction::CompactionPolicy::from_env();
        let compact_threshold = compaction.threshold(&base_opts.model);
        let context_window = compaction.context_window(&base_opts.model);

        // Timestamp the session boundary in the sidecar log. Daemon-launched
        // workers receive the dispatch provider through
        // `BRO_HARNESS_PROVIDER`; direct launches fall back to
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

        let mut session = Self {
            tx,
            reg,
            code_mode,
            code_mode_session,
            remote_outcome_sources,
            retain_background_work: cli.input_format.as_deref() == Some("stream-json")
                && !cli.exit_when_idle,
            cx,
            reference_context_item,
            hooks,
            scoped_project_docs,
            resume_runtime_reset: restored_snapshot,
            emitter,
            base_opts,
            explicit_system,
            instruction_system: None,
            max_turns,
            compaction,
            compact_threshold,
            context_window,
            tool_result_cap,
            strategy,
            dispatch,
            store,
            event_log,
            seq_counter,
            prior_side,
            pending_user_inputs,
            todos,
            lsp_baselines,
            lsp_pool: bro_lsp::SessionPool::new(lsp_config),
            lsp_documents: BTreeMap::new(),
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: restored_budget.last_input_tokens,
            pending_input_estimate: restored_budget.pending_tokens,
            last_request_overhead_tokens: restored_budget.overhead_tokens,
            tail_nudge: None,
            output_schema,
        };
        if let Some(previous_model) = restored_model
            && previous_model != session.base_opts.model
        {
            let requested_model = session.base_opts.model.clone();
            session.base_opts.model = previous_model;
            session.context_window = session.compaction.context_window(&session.base_opts.model);
            session.compact_threshold = session.compaction.threshold(&session.base_opts.model);
            let transition = session.apply_control(&requested_model).await;
            // Startup compaction and cancellation observations must share the
            // checkpoint even if changing models is rejected.
            session.persist().await?;
            transition?;
        }
        Ok(session)
    }

    fn session_id(&self) -> &str {
        self.emitter.session_id()
    }

    /// Shared handle to the sidecar event log, for auxiliary emitters
    /// (control responses, stdin replay) created outside `build`.
    fn event_log(&self) -> Arc<EventLog> {
        self.event_log.clone()
    }

    /// Shared per-session event seq counter, for auxiliary emitters (control
    /// responses, stdin replay) created outside `build`; see
    /// `make_emitter`'s doc comment for why every emitter for this session
    /// must share the same counter.
    fn seq_counter(&self) -> Arc<AtomicU64> {
        self.seq_counter.clone()
    }

    /// Fit history with the previous model before committing a smaller model.
    async fn apply_control(&mut self, model: &str) -> Result<()> {
        let next_window = self.compaction.context_window(model);
        let next_threshold = self.compaction.threshold(model);
        if model != self.base_opts.model
            && let (Some(previous_window), Some(window)) = (self.context_window, next_window)
            && previous_window > window
        {
            self.reg.validate_resume_tool_schemas()?;
            let tools = self.reg.wire_specs();
            let opts = TurnOpts {
                system: compose_system(
                    &self.system_sections(),
                    &self.reg,
                    self.output_schema.is_some(),
                ),
                ..self.base_opts.clone()
            };
            let projected = self.projected_request_tokens(&tools, &opts);
            let limit = next_threshold
                .unwrap_or(window)
                .min(window.saturating_sub(u64::from(opts.max_tokens)));
            if projected > limit {
                self.drain_cancelled_work().await;
                // Cancellation can add durable outcome observations to history.
                let projected = self.projected_request_tokens(&tools, &opts);
                self.event_log.append_milestone("compaction_start", self.emitter.session_id(),
                    json!({"reason":"model_change", "from":self.base_opts.model, "to":model, "projected_tokens":projected}));
                let summary = self
                    .tx
                    .compact(
                        self.compaction.params(),
                        crate::compaction::COMPACTION_INSTRUCTION,
                        &tools,
                        &opts,
                    )
                    .await
                    .context("compact with previous model before downshift")?
                    .context("history cannot be compacted before model downshift")?;
                self.emitter
                    .compact_boundary("model_change", projected, summary.len());
                self.reset_compaction_context();
                self.deliver_instruction_context().await?;
                self.prepare_context_for_user_turn();
                let destination_opts = TurnOpts {
                    model: model.to_owned(),
                    system: compose_system(
                        &self.system_sections(),
                        &self.reg,
                        self.output_schema.is_some(),
                    ),
                    ..self.base_opts.clone()
                };
                let added = self.tx.prepare_request_context(&destination_opts);
                self.pending_input_estimate = self.pending_input_estimate.saturating_add(added);
                anyhow::ensure!(
                    self.projected_request_tokens(&tools, &destination_opts) <= limit,
                    "compacted history still exceeds destination model budget; previous model retained"
                );
            }
        }
        if model != self.base_opts.model {
            // Usage was measured with the previous model's tokenizer. Estimate
            // the retained history until the new model reports its first input.
            self.last_prompt_tokens = 0;
            self.pending_input_estimate = 0;
            self.last_request_overhead_tokens = 0;
        }
        self.base_opts.model = model.to_owned();
        self.compact_threshold = next_threshold;
        self.context_window = next_window;
        tracing::info!(model, "set_model");
        Ok(())
    }

    fn projected_request_tokens(&self, tools: &[transport::ToolSpec], opts: &TurnOpts) -> u64 {
        crate::context::budget::RequestEstimate::new(&self.tx.snapshot(), tools, opts)
            .projected(&self.budget_checkpoint())
    }

    fn budget_checkpoint(&self) -> crate::context::budget::BudgetCheckpoint {
        crate::context::budget::BudgetCheckpoint::new(
            self.last_prompt_tokens,
            self.pending_input_estimate,
            self.last_request_overhead_tokens,
        )
    }

    fn reset_compaction_context(&mut self) {
        self.last_prompt_tokens = 0;
        self.pending_input_estimate = 0;
        self.last_request_overhead_tokens = 0;
        self.reference_context_item = None;
        self.scoped_project_docs.invalidate_delivery();
    }

    /// Manual `/compact`: summarize-and-replace the prefix and emit a manual
    /// `compact_boundary`. A no-op (logged) when there isn't enough history.
    async fn compact_manual(&mut self) -> Result<()> {
        self.reg.validate_resume_tool_schemas()?;
        let tool_specs = self.reg.wire_specs();
        let opts = TurnOpts {
            system: compose_system(
                &self.system_sections(),
                &self.reg,
                self.output_schema.is_some(),
            ),
            ..self.base_opts.clone()
        };
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
                &opts,
            )
            .await?
        {
            Some(summary) => {
                self.emitter
                    .compact_boundary("manual", self.last_prompt_tokens, summary.len());
                self.reset_compaction_context();
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
        cancel: watch::Receiver<bool>,
        mid_turn_user_inputs: Arc<StdMutex<VecDeque<String>>>,
    ) -> Result<()> {
        let result = self
            .user_turn_inner(prompt, cancel, mid_turn_user_inputs)
            .await;
        if let Err(error) = &result {
            self.drain_cancelled_work().await;
            if let Some(observation) = error.downcast_ref::<transport::FailedTurnObservation>() {
                self.emitter.failed_turn_observation(observation);
            }
            self.emitter.result_error(&format!("{error:#}"), self.turns);
            let log = self.event_log.clone();
            let _ = tokio::task::spawn_blocking(move || log.flush_blocking()).await;
        }
        result
    }

    async fn user_turn_inner(
        &mut self,
        prompt: &str,
        mut cancel: watch::Receiver<bool>,
        mid_turn_user_inputs: Arc<StdMutex<VecDeque<String>>>,
    ) -> Result<()> {
        self.reg.validate_resume_tool_schemas()?;
        self.cx.cancellation = tokio_util::sync::CancellationToken::new();
        let mut pending_prompt = Some(prompt);
        let prompt_estimate = est_tokens(prompt);
        let mut request_overhead_tokens = self.last_request_overhead_tokens;

        let mut final_text = String::new();
        // The TERMINAL step's text, tracked separately from `final_text`:
        // `final_text` keeps the last NON-EMPTY text of the whole turn (it is
        // the result payload), so it cannot detect a model that ends on an
        // output-free step — any earlier narration masks it (gap-aa032081).
        let mut last_step_text = String::new();
        // One-shot guard for the empty-output nudge below.
        let mut empty_output_nudged = false;
        let mut tool_batch_correction_reason: Option<String> = None;
        let mut turn_steps = 0u64;
        let mut last_model_stop: Option<StopReason> = None;
        let mut last_model_tool_call_count = 0usize;
        let mut last_tool_results: Vec<Value> = Vec::new();

        let break_reason = 'turn: loop {
            if !self.uncertain_remote_outcomes().is_empty() {
                break "remote_outcome_unknown";
            }
            if turn_steps >= self.max_turns {
                tracing::warn!(max_turns = self.max_turns, "hit max turns; stopping");
                break "max_turns";
            }
            if *cancel.borrow() {
                break "cancelled";
            }

            let tool_specs = self.reg.wire_specs();

            // Preserve the same tail across an overflow retry while rebuilding
            // the strategy's system section from the refreshed instruction state.
            let mut tail_nudge = None;

            // Run the model call, recovering once from a context-window
            // rejection by compacting and retrying. This is the reactive safety
            // net for the case the proactive threshold check above misses: a
            // single step (e.g. a large tool result) jumping over the window in
            // one shot. Without it, the typed `ContextWindowExceeded` would fail
            // the whole turn instead of self-healing.
            let mut overflow_compacted = false;
            let mut proactive_checked = false;
            let out = 'attempt: loop {
                self.deliver_instruction_context().await?;
                if pending_prompt.is_some() || self.reference_context_item.is_none() {
                    self.prepare_context_for_user_turn();
                }
                if let Some(nudge) = self.tail_nudge.take() {
                    tail_nudge = Some(nudge);
                }
                let mut sys = compose_system(
                    &self.system_sections(),
                    &self.reg,
                    self.output_schema.is_some(),
                );
                if let Some(t) = &tail_nudge {
                    let v = sys.volatile.get_or_insert_with(String::new);
                    if !v.is_empty() {
                        v.push('\n');
                    }
                    v.push_str(t);
                }
                // Per-turn directives ride the volatile lane AFTER the existing
                // channels (structured-output reminder, tail nudges), design §8.
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
                let added = self.tx.prepare_request_context(&opts);
                self.pending_input_estimate = self.pending_input_estimate.saturating_add(added);
                let estimate = crate::context::budget::RequestEstimate::new(
                    &self.tx.snapshot(),
                    &tool_specs,
                    &opts,
                );
                let queued_estimate = mid_turn_user_inputs
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .map(|prompt| est_tokens(prompt))
                    .fold(0u64, u64::saturating_add);
                let projected = estimate
                    .projected(&self.budget_checkpoint())
                    .saturating_add(queued_estimate)
                    .saturating_add(if pending_prompt.is_some() {
                        prompt_estimate
                    } else {
                        0
                    });
                if !proactive_checked {
                    proactive_checked = true;
                    if self
                        .compact_threshold
                        .is_some_and(|threshold| projected > threshold)
                    {
                        self.event_log.append_milestone(
                            "compaction_start",
                            self.emitter.session_id(),
                            json!({"reason":"auto", "projected_tokens":projected}),
                        );
                        match self
                            .tx
                            .compact(
                                self.compaction.params(),
                                crate::compaction::COMPACTION_INSTRUCTION,
                                &tool_specs,
                                &opts,
                            )
                            .await
                        {
                            Ok(Some(summary)) => {
                                self.emitter
                                    .compact_boundary("auto", projected, summary.len());
                                self.reset_compaction_context();
                                continue 'attempt;
                            }
                            Ok(None) => {}
                            Err(error) => tracing::warn!("compaction failed: {error:#}"),
                        }
                    }
                }
                if let Some(prompt) = pending_prompt.take() {
                    // Record the task at the same boundary/order seen by the
                    // model, after authoritative context has been delivered.
                    self.event_log.append_event(&json!({
                        "type": "user",
                        "session_id": self.session_id(),
                        "message": {
                            "role": "user",
                            "content": [{"type": "text", "text": prompt}],
                        },
                    }));
                    self.push_user_text_raw(prompt);
                }
                self.drain_mid_turn_user_inputs(&mid_turn_user_inputs)
                    .await?;
                // A queued manual compaction invalidates the context/options
                // assembled above. Restore current instructions before sampling.
                if self.reference_context_item.is_none() {
                    continue 'attempt;
                }
                self.tx.normalize_for_prompt();
                request_overhead_tokens = estimate.overhead_tokens;
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
                            && e.downcast_ref::<transport::FailedTurnObservation>()
                                .is_none()
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
                                &opts,
                            )
                            .await
                        {
                            Ok(Some(summary)) => {
                                self.emitter.compact_boundary(
                                    "overflow",
                                    self.last_prompt_tokens,
                                    summary.len(),
                                );
                                self.reset_compaction_context();
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
            // Publish window occupancy every step. The terminal `result` event
            // reports it too late to be actionable: an orchestrator needs to
            // rotate the session BEFORE the provider rejects a prompt for
            // exceeding the window, and by `result` the session is over.
            self.emitter.context_pressure(
                self.last_prompt_tokens,
                self.context_window,
                self.compact_threshold,
            );
            // The response is retained history for the next request. Provider
            // output usage includes reasoning, which visible text alone misses.
            self.pending_input_estimate = out.usage.output_tokens;
            self.last_request_overhead_tokens = request_overhead_tokens;
            last_model_stop = Some(out.stop.clone());
            last_model_tool_call_count = out.tool_calls.len();
            last_step_text = out.text.clone();

            for n in self.hooks.on_assistant_turn(&out.text, &out.tool_calls) {
                if n.delivery == Delivery::SystemTail {
                    self.tail_nudge = Some(n.message);
                }
            }

            // Preserve native block order and provider-owned tool results in
            // the durable event. Only out.tool_calls enter client dispatch.
            if !out.text.is_empty() {
                final_text = out.text.clone();
            }
            let assistant_content = out.observation_content.clone().unwrap_or_else(|| {
                let mut assistant_content: Vec<Value> = Vec::new();
                if !out.thinking.is_empty() {
                    assistant_content.push(json!({"type": "thinking", "thinking": out.thinking}));
                }
                if !out.text.is_empty() {
                    assistant_content.push(json!({"type": "text", "text": out.text}));
                }
                for tc in &out.tool_calls {
                    assistant_content.push(json!({
                        "type": "tool_use",
                        "id": tc.id,
                        "name": tc.name,
                        "input": tc.args,
                    }));
                }
                assistant_content
            });
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

            // Incomplete provider responses cannot authorize any accumulated calls.
            if !matches!(out.stop, StopReason::Done | StopReason::ToolCalls) {
                let results: Vec<_> = out
                    .tool_calls
                    .iter()
                    .map(|call| transport::ToolResult {
                        id: call.id.clone(),
                        content: "Tool not executed: provider response did not complete normally."
                            .into(),
                        is_error: true,
                    })
                    .collect();
                if !results.is_empty() {
                    self.emitter.tool_results(&results);
                    self.tx.push_tool_results(results);
                }
                break if out.stop == StopReason::Length {
                    "output_limit"
                } else {
                    "provider_stop"
                };
            }

            // A rejected batch stays visible and replayable, but no client
            // action (including final_result) runs. Supply a result for every
            // ID before either requesting correction or terminating.
            let batch_rejection = if let Some(original_reason) = &tool_batch_correction_reason
                && out.tool_calls.len() > 1
            {
                Some(format!(
                    "{original_reason} Correction protocol violation: this response contains {} client calls; at most one client call per response is permitted for the remainder of this user turn. None of the client tools in this correction batch were executed.",
                    out.tool_calls.len()
                ))
            } else {
                self.tx.tool_batch_rejection(&out).or_else(|| {
                    final_result_rejection(self.output_schema.as_ref(), &out.tool_calls)
                })
            };
            if let Some(reason) = batch_rejection {
                last_tool_results = out
                    .tool_calls
                    .iter()
                    .map(|call| {
                        json!({
                            "id": call.id, "name": call.name, "is_error": true, "rejected": true,
                        })
                    })
                    .collect();
                let results: Vec<_> = out
                    .tool_calls
                    .iter()
                    .map(|call| transport::ToolResult {
                        id: call.id.clone(),
                        content: reason.clone(),
                        is_error: true,
                    })
                    .collect();
                self.emitter.tool_results(&results);
                self.pending_input_estimate = self
                    .pending_input_estimate
                    .saturating_add(est_tool_results(&results));
                self.tx.push_tool_results(results);
                self.hooks.tick();
                if *cancel.borrow() {
                    break 'turn "cancelled";
                }
                if tool_batch_correction_reason.is_some() {
                    anyhow::bail!(
                        "{reason} Automatic correction was already attempted for this user turn; stopping."
                    );
                }
                if turn_steps >= self.max_turns {
                    anyhow::bail!(
                        "{reason} No correction turn remains within the configured turn limit; stopping."
                    );
                }
                tool_batch_correction_reason = Some(reason);
                self.drain_mid_turn_user_inputs(&mid_turn_user_inputs)
                    .await?;
                continue;
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

            // Admission above requires a single schema-valid terminal call.
            if self.output_schema.is_some()
                && let Some(fr) = out
                    .tool_calls
                    .first()
                    .filter(|tc| tc.name == FINAL_RESULT_TOOL)
            {
                let text = serde_json::to_string(&fr.args)?;
                let result = transport::ToolResult {
                    id: fr.id.clone(),
                    content: text.clone(),
                    is_error: false,
                };
                self.emitter.tool_results(std::slice::from_ref(&result));
                self.tx.push_tool_results(vec![result]);
                final_text = text.clone();
                last_step_text = text;
                break "structured_result";
            }

            // Preserve the model's call order across mutations. Only adjacent
            // reads may overlap; a write is a barrier for reads on either side.
            // Collect each completed read immediately so cancellation of a
            // sibling cannot erase an already completed result.
            let call_count = out.tool_calls.len();
            let mut raw: Vec<Option<(String, bool)>> = (0..call_count).map(|_| None).collect();
            last_tool_results.clear();
            let mut interrupted = false;
            let mut next = 0;
            while next < call_count {
                if *cancel.borrow() {
                    interrupted = true;
                    break;
                }
                if self.reg.read_only(&out.tool_calls[next].name) {
                    use futures_util::StreamExt as _;
                    let start = next;
                    while next < call_count && self.reg.read_only(&out.tool_calls[next].name) {
                        next += 1;
                    }
                    let reg = &self.reg;
                    let cx = &self.cx;
                    let calls = &out.tool_calls;
                    let mut pending: futures_util::stream::FuturesUnordered<_> = (start..next)
                        .map(|i| async move {
                            let tc = &calls[i];
                            (i, reg.dispatch(&tc.name, tc.args.clone(), cx).await)
                        })
                        .collect();
                    while !pending.is_empty() {
                        tokio::select! {
                            biased;
                            done = pending.next() => {
                                if let Some((i, res)) = done {
                                    raw[i] = Some(res.into_content());
                                }
                            }
                            _ = cancel.changed() => {
                                interrupted = true;
                                self.cx.cancellation.cancel();
                                while let Some((i, res)) = pending.next().await {
                                    raw[i] = Some(res.into_content());
                                }
                                break;
                            }
                        }
                    }
                    if interrupted {
                        break;
                    }
                } else {
                    let tc = &out.tool_calls[next];
                    tracing::info!(tool = %tc.name, "dispatch");
                    let dispatch = self.reg.dispatch(&tc.name, tc.args.clone(), &self.cx);
                    tokio::pin!(dispatch);
                    let result = tokio::select! {
                        biased;
                        result = &mut dispatch => result,
                        _ = cancel.changed() => {
                            interrupted = true;
                            self.cx.cancellation.cancel();
                            dispatch.await
                        }
                    };
                    raw[next] = Some(result.into_content());
                    next += 1;
                    if interrupted {
                        break;
                    }
                }
            }

            interrupted |= *cancel.borrow();

            // Assemble results in tool-call order: bound oversized output, run
            // result hooks. Diagnostics are deferred to the single batch-boundary
            // pass below — the per-edit window-0 drain could not attribute edits
            // under concurrent dispatch or V8 code-mode cells.
            let mut results: Vec<transport::ToolResult> = Vec::with_capacity(call_count);
            let mut tool_context = String::new();
            for (i, tc) in out.tool_calls.iter().enumerate() {
                let Some((content, is_error)) = raw[i].take() else {
                    continue;
                };
                // Bound producer output before appending contextual riders.
                let content =
                    crate::bound::bound_tool_result(&tc.name, content, self.tool_result_cap);
                let result = transport::ToolResult {
                    id: tc.id.clone(),
                    content,
                    is_error,
                };
                for n in self.hooks.on_tool_result(tc, &result) {
                    match n.delivery {
                        Delivery::Rider => {
                            tool_context.push_str(&format!(
                                "\nContext for tool result {}:{}",
                                tc.id,
                                n.rider_block()
                            ));
                        }
                        Delivery::SystemTail => self.tail_nudge = Some(n.message),
                    }
                }
                results.push(result);
            }

            // Batch-boundary diagnostics: one analyzer pass over every edit this
            // dispatch round produced, attached to the last result so it rides
            // back with the batch. Replaces the per-tool window-0 drain (which
            // could not attribute edits under concurrent dispatch / V8 cells).
            if !interrupted && !results.is_empty() {
                self.append_edit_diagnostics(&mut tool_context).await;
            }

            // Ordinary hooks and diagnostics share the result budget. Scoped
            // instruction documents must survive intact, so append them after
            // bounding. Trace the final payload that actually reaches the model.
            for result in &mut results {
                result.content = crate::bound::bound_tool_result(
                    "tool_result",
                    std::mem::take(&mut result.content),
                    self.tool_result_cap,
                );
                if let Some(tc) = out.tool_calls.iter().find(|tc| tc.id == result.id) {
                    last_tool_results.push(tool_result_trace(tc, result));
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
            }

            self.emitter.tool_results(&results);
            self.pending_input_estimate = self
                .pending_input_estimate
                .saturating_add(est_tool_results(&results));
            self.tx.push_tool_results(results);
            if !tool_context.is_empty() {
                // Shell pages already consumed exactly the bytes delivered in
                // their JSON. Context must not displace that non-replayable data.
                let content = crate::bound::bound_tool_result(
                    "tool_context",
                    tool_context,
                    self.tool_result_cap,
                );
                self.emitter.tool_result_context(&content);
                self.pending_input_estimate = self
                    .pending_input_estimate
                    .saturating_add(est_tokens(&content));
                self.tx.push_user_text(&content);
            }
            if interrupted {
                break "interrupted_dispatch";
            }
            self.drain_mid_turn_user_inputs(&mid_turn_user_inputs)
                .await?;
            self.hooks.tick();
        };

        // Even an early cancellation or a zero-step budget must preserve the
        // accepted, logged input in the snapshot before checkpointing its event.
        if let Some(prompt) = pending_prompt.take() {
            self.event_log.append_event(&json!({
                "type":"user", "session_id":self.session_id(),
                "message":{"role":"user", "content":[{"type":"text", "text":prompt}]},
            }));
            self.prepare_context_for_user_turn();
            self.push_user_text_raw(prompt);
        }

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
                    self.pending_input_estimate = partial.output_tokens;
                    self.last_request_overhead_tokens = request_overhead_tokens;
                }
            }
            self.tx.note_interrupted();
            self.drain_cancelled_work().await;
        } else if !self.retain_background_work {
            self.drain_cancelled_work().await;
        }

        self.deliver_tool_observations();
        let break_reason = if self.uncertain_remote_outcomes().is_empty() {
            break_reason
        } else {
            "remote_outcome_unknown"
        };
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
                (self.last_prompt_tokens > 0).then_some(self.last_prompt_tokens),
            );
        } else {
            let incomplete = matches!(
                break_reason,
                "max_turns"
                    | "output_limit"
                    | "provider_stop"
                    | "tool_calls_empty"
                    | "remote_outcome_unknown"
            ) || (break_reason == "model_stop"
                && last_step_text.trim().is_empty())
                || (self.output_schema.is_some() && break_reason != "structured_result");
            let mut terminal = turn_end.clone();
            terminal["incomplete"] = json!(incomplete);
            if self.output_schema.is_some() && break_reason != "structured_result" {
                terminal["missing_structured_result"] = json!(true);
            }
            self.emitter.result(
                &last_step_text,
                &self.total_usage,
                self.turns,
                None,
                (suspicious || incomplete).then_some(&terminal),
                self.compact_threshold,
                (self.last_prompt_tokens > 0).then_some(self.last_prompt_tokens),
            );
        }
        // Drain the sidecar event-log writer at the turn boundary — bounds
        // the crash-durability gap to the current turn while keeping
        // per-event appends off the runtime workers (event_log.rs).
        let log = self.event_log.clone();
        let _ = tokio::task::spawn_blocking(move || log.flush_blocking()).await;
        Ok(())
    }

    /// Stop admission to yielded work and retain completion facts before the
    /// interrupted turn is acknowledged. Blocking mutations may delay this.
    async fn drain_cancelled_work(&mut self) {
        self.cx.cancellation.cancel();
        let cells = if let Some(session) = &self.code_mode_session {
            session
                .cancel_all()
                .await
                .into_iter()
                .map(|result| {
                    let (content, is_error) = result.into_content();
                    json!({"content": content, "is_error": is_error})
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        let shells = bro_tools::shell::shutdown_shell_sessions(&self.cx).await;
        let edits = self
            .cx
            .edits
            .lock()
            .map(|mut sink| sink.drain())
            .unwrap_or_default();
        let recorded_changes: Vec<_> = edits
            .into_iter()
            .map(|edit| {
                json!({
                    "path": edit.path,
                    "pre_sha256": edit.pre_sha256,
                    "post_sha256": edit.post_sha256,
                })
            })
            .collect();
        self.deliver_tool_observations();
        if cells.is_empty() && shells.is_empty() && recorded_changes.is_empty() {
            return;
        }
        let content = format!(
            "[Tool outcomes after cancellation]\n{}",
            json!({
                "cells": cells,
                "shells": shells,
                "recorded_file_changes": recorded_changes,
            })
        );
        let content =
            crate::bound::bound_tool_result("cancellation_outcomes", content, self.tool_result_cap);
        // Earlier yielded exec/shell calls already have tool results. This is
        // a context observation, not a second result for those request IDs.
        self.emitter.cancellation_outcomes(&content);
        self.pending_input_estimate = self
            .pending_input_estimate
            .saturating_add(est_tokens(&content));
        self.tx.push_user_text(&content);
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
            sections.memory = self.instruction_system.clone();
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
            // The typed instruction batch is delivered first at the request
            // boundary; scope, pins and environment follow before the task.
            if self.dispatch.scope_render().is_none() && self.dispatch.emitted_scope.is_some()
                || self.dispatch.pins_render().is_none() && self.dispatch.emitted_pins.is_some()
            {
                self.emit_dispatch_context_changes_if_needed();
            }
            let mut sections = Vec::new();
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

    /// Replacements and removals both update authoritative context.
    fn emit_dispatch_context_changes_if_needed(&mut self) {
        let mut sections = Vec::new();
        let scope = self.dispatch.scope_render();
        if scope != self.dispatch.emitted_scope {
            sections.push(scope.clone().unwrap_or_else(|| {
                "<bbox_scope>Prior dispatch scope has been cleared.</bbox_scope>".into()
            }));
            self.dispatch.emitted_scope = scope;
        }
        let pins = self.dispatch.pins_render();
        if pins != self.dispatch.emitted_pins {
            sections.push(pins.clone().unwrap_or_else(|| {
                "<bbox_pins>Prior dispatch pins have been cleared.</bbox_pins>".into()
            }));
            self.dispatch.emitted_pins = pins;
        }
        if let Some(message) = crate::context::build_contextual_user_message(sections) {
            let text = message.text_blocks.join("\n\n");
            self.pending_input_estimate = self
                .pending_input_estimate
                .saturating_add(est_tokens(&text));
            self.emitter.tool_result_context(&text);
            self.tx.push_user_text_blocks(message.text_blocks);
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

    async fn drain_mid_turn_user_inputs(
        &mut self,
        inputs: &Arc<StdMutex<VecDeque<String>>>,
    ) -> Result<()> {
        // Take one input at a time. Unconsumed inputs survive an error and no
        // std mutex is held across a compaction request.
        loop {
            let prompt = inputs
                .lock()
                .map_err(|_| anyhow::anyhow!("input queue poisoned"))?
                .pop_front();
            let Some(prompt) = prompt else {
                return Ok(());
            };
            self.event_log.append_event(&json!({
                "type": "user", "session_id": self.session_id(),
                "message": {"role": "user", "content": [{"type":"text", "text":prompt}]},
            }));
            if prompt.trim() == "/compact" {
                self.compact_manual().await?;
            } else {
                self.push_user_text_raw(&prompt);
            }
        }
    }

    fn deliver_tool_observations(&mut self) {
        let batch = self.cx.tool_observations.drain();
        if batch.observations.is_empty() && batch.dropped_observations == 0 {
            return;
        }
        let content = format!(
            "[Host tool argument policy observations]\n{}",
            serde_json::to_string(&batch).expect("tool observations serialize")
        );
        self.emitter.tool_policy_observations(&batch, &content);
        self.pending_input_estimate = self
            .pending_input_estimate
            .saturating_add(est_tokens(&content));
        self.tx.push_user_text(&content);
    }

    async fn deliver_instruction_context(&mut self) -> Result<()> {
        self.deliver_tool_observations();
        if std::mem::take(&mut self.resume_runtime_reset) {
            let reset = "[Session runtime reset] This is a resumed process. Previous JavaScript cells, store/load values, and shell sessions are unavailable. Durable files and recorded conversation remain; inspect files before retrying prior effects. Current host instructions and environment follow.";
            self.tx.push_user_text(reset);
            self.emitter.tool_result_context(reset);
            self.pending_input_estimate = self
                .pending_input_estimate
                .saturating_add(est_tokens(reset));
        }
        self.scoped_project_docs
            .refresh()
            .await
            .map_err(anyhow::Error::msg)?;
        self.cx.instruction_generation = self
            .cx
            .instruction_generation
            .checked_add(1)
            .context("instruction generation exhausted")?;
        if let Some(batch) = self
            .scoped_project_docs
            .pending_batch(self.cx.instruction_generation)
        {
            let batch = if self.strategy.context_rides_user_lane() {
                self.tx.push_user_text(&batch.text);
                self.pending_input_estimate = self
                    .pending_input_estimate
                    .saturating_add(est_tokens(&batch.text));
                batch
            } else {
                let snapshot = self.scoped_project_docs.snapshot_batch(batch.generation);
                self.instruction_system = Some(snapshot.text.clone());
                snapshot
            };
            // These exact bytes are now captured in transport history or the
            // leading system section used by the imminent request. Tool output
            // and old yielded cells cannot acknowledge this generation.
            self.emitter
                .instruction_context(batch.generation, &batch.text, &batch.documents);
            self.scoped_project_docs.acknowledge(&batch);
        }
        Ok(())
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

    /// Capture native/side state at a quiescent boundary, then flush the log,
    /// checkpoint its exact byte length, and write through the shared serializer.
    /// All filesystem work happens in this single awaited blocking task.
    #[allow(
        clippy::disallowed_methods,
        reason = "filesystem calls execute inside spawn_blocking"
    )]
    async fn persist(&mut self) -> Result<()> {
        // Retained cells may finish while this checkpoint is written. A cell
        // with unconsumed terminal output remains in the service catalog, and
        // any cell observed here conservatively marks this entire checkpoint.
        // No new flat execution is admitted while this Session is borrowed.
        let cells_outstanding = if let Some(session) = &self.code_mode_session {
            session.has_outstanding_work().await
        } else {
            false
        };
        let shells_outstanding = !self
            .cx
            .shell_sessions
            .lock()
            .map_err(|_| anyhow::anyhow!("shell session map poisoned during checkpoint"))?
            .is_empty();
        let transport = self.tx.name().to_owned();
        let model = self.base_opts.model.clone();
        let code_mode = self.code_mode.as_str().to_owned();
        let service_tier = self.base_opts.service_tier.clone();
        let effort = self.base_opts.effort.clone();
        let snapshot = self.tx.snapshot();
        let mut side = self.side_state();
        side["runtime_work_outstanding"] = json!(cells_outstanding || shells_outstanding);
        let last_event_seq = self.seq_counter.load(Ordering::SeqCst);
        let path = self.store.store_path().clone();
        let writer_lease = self.store.writer_lease();
        let log = self.event_log.clone();
        tokio::task::spawn_blocking(move || {
            let _writer_lease = writer_lease;
            log.flush_blocking_checked()?;
            let event_log_offset = if log.path() == path.with_extension("events.jsonl") {
                match std::fs::metadata(log.path()) {
                    Ok(metadata) => Some(metadata.len()),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(0),
                    Err(error) => return Err(error).context("read flushed event-log checkpoint"),
                }
            } else {
                // Disabled/custom logs are test seams, not the canonical resume log.
                None
            };
            let body = SessionStore::serialize(
                &crate::session::SaveState {
                    transport: &transport,
                    model: &model,
                    code_mode: &code_mode,
                    service_tier: service_tier.as_deref(),
                    effort: effort.as_deref(),
                    snapshot,
                    side,
                    last_event_seq,
                },
                event_log_offset,
            )?;
            crate::session::write_atomic(&path, &body).context("write session")
        })
        .await
        .context("persist task panicked")?
    }

    fn uncertain_remote_outcomes(&self) -> Vec<String> {
        self.remote_outcome_sources
            .iter()
            .filter_map(|tool| tool.uncertain_outcome())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    fn side_state(&self) -> Value {
        // Flush loop-level cells back into `side`, preserving any slots this
        // build doesn't own.
        let mut side = match self.prior_side.clone() {
            Value::Object(m) => Value::Object(m),
            _ => json!({}),
        };
        side["context_budget"] =
            serde_json::to_value(self.budget_checkpoint()).expect("budget checkpoint serializes");
        side["pending_user_inputs"] = json!(self.pending_user_inputs);
        side["todos"] = self
            .todos
            .lock()
            .map(|t| t.to_side())
            .unwrap_or(Value::Null);
        side["nudges"] = self.hooks.to_side();
        side["tool_activations"] = self.reg.activation_state();
        side["lsp_baselines"] = self.lsp_baselines.to_side();
        side["reference_context"] = self
            .reference_context_item
            .as_ref()
            .map(crate::context::TurnContextItem::to_side)
            .unwrap_or(Value::Null);
        side["instruction_observed_paths"] =
            serde_json::to_value(self.scoped_project_docs.observed_paths())
                .expect("instruction paths serialize");
        side["instruction_documents"] =
            serde_json::to_value(self.scoped_project_docs.active_documents())
                .expect("instruction documents serialize");
        side["remote_outcomes_unknown"] = json!(self.uncertain_remote_outcomes());
        side["output_schema"] = self.output_schema.clone().unwrap_or(Value::Null);
        side["dispatch_context"] = self.dispatch.context_to_side();
        side["dispatch_emitted"] = self.dispatch.emitted_to_side();
        side
    }
}

fn reference_context_item_for_restore(
    _restored_snapshot: bool,
    _restored_reference_context: Option<crate::context::TurnContextItem>,
    _cx: &ToolCx,
) -> Option<crate::context::TurnContextItem> {
    // A new process re-establishes current host context, including explicit
    // null and legacy baselines. Process-local work is never restored here.
    None
}

fn load_tool_arg_defaults(
    explicit: Option<BTreeMap<String, Value>>,
    cli_json: Option<&str>,
) -> Result<bro_tools::ToolArgDefaults> {
    let raw = match explicit {
        Some(map) => map,
        None => match cli_json {
            Some(raw) => serde_json::from_str::<BTreeMap<String, Value>>(raw)
                .context("parse --additional-context as JSON value map")?,
            None => match std::env::var("BRO_HARNESS_TOOL_DEFAULTS") {
                Ok(raw) if !raw.trim().is_empty() => {
                    serde_json::from_str::<BTreeMap<String, Value>>(&raw)
                        .context("parse BRO_HARNESS_TOOL_DEFAULTS as JSON value map")?
                }
                _ => BTreeMap::new(),
            },
        },
    };
    bro_tools::ToolArgDefaults::parse_values(raw)
        .map_err(anyhow::Error::msg)
        .context("parse tool arg default table")
}

/// Host-supplied shell env overlay: explicit library-call map > `--shell-env`
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

/// Rough UTF-8 byte estimate (~4 bytes/token) for the compaction trigger.
/// Deliberately coarse: it only needs to flag an appended item large enough to
/// push the next request over the window, and the threshold leaves headroom.
fn est_tokens(s: &str) -> u64 {
    crate::context::budget::text_tokens(s)
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
                 `0` waits until exit/timeout). If `shell_run` or `shell_poll` returns `running=true` or `output_pending=true`, \
                 call `shell_poll` with the returned `session_id` \
                 until both `running=false` and `output_pending=false` (an exited command may still have unread pages), or `shell_kill` \
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

    /// An embedded caller's session env must beat process env for
    /// BRO_HARNESS_WEB_SEARCH, while the standalone-binary path still reads
    /// process env. Daemon workers use the latter in their isolated child env.
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
        NativeSearch(Vec<Value>),
        Terminal(StopReason, String, Vec<transport::ToolCall>),
        HighUsageFollowUp,
        ContextOverflow,
        Failure,
        /// Return text immediately with a Responses-style follow-up signal.
        TextWithEndTurn(String, Option<bool>),
        /// Wait on the shared gate, then request a tool call.
        ToolCallAfterGate,
        /// Request two read-only `concurrent_probe` calls in one batch (to prove
        /// they dispatch concurrently).
        TwoReadProbes,
        /// Request a caller-supplied batch for dispatch contract tests.
        ToolCalls(Vec<transport::ToolCall>),
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
        compact_block: Arc<std::sync::atomic::AtomicBool>,
        compact_gate: Arc<Notify>,
        compact_fail: Arc<std::sync::atomic::AtomicBool>,
        model_gate: Arc<Notify>,
        tool_started: Arc<AtomicUsize>,
        tool_gate: Arc<Notify>,
        /// Count of read-only probe calls that rendezvoused at the shared
        /// barrier — only reaches 2 if the batch ran concurrently (phase 1).
        rendezvous: Arc<AtomicUsize>,
        /// SystemPrompt observed by each run_turn call, for slot-routing
        /// assertions (volatile-lane ordering, stable composition).
        seen_systems: Arc<Mutex<Vec<SystemPrompt>>>,
        seen_users: Arc<Mutex<Vec<Vec<String>>>>,
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
                .seen_users
                .lock()
                .unwrap()
                .push(self.shared.pushed_users.lock().unwrap().clone());
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
                MockTurn::Terminal(stop, text, tool_calls) => Ok(transport::TurnOutput {
                    observation_content: None,
                    text,
                    thinking: String::new(),
                    tool_calls,
                    stop,
                    end_turn: None,
                    usage: Usage::default(),
                }),
                MockTurn::ContextOverflow => {
                    Err(transport::ContextWindowExceeded("test overflow".into()).into())
                }
                MockTurn::HighUsageFollowUp => Ok(transport::TurnOutput {
                    observation_content: None,
                    text: "continuing".into(),
                    thinking: String::new(),
                    tool_calls: vec![],
                    stop: StopReason::Done,
                    end_turn: Some(false),
                    usage: Usage {
                        input_tokens: 10_000,
                        ..Usage::default()
                    },
                }),
                MockTurn::Failure => anyhow::bail!("synthetic provider failure"),
                MockTurn::Block => {
                    self.shared.model_gate.notified().await;
                    unreachable!("gate is never released in tests");
                }
                MockTurn::ToolCallAfterGate => {
                    self.shared.model_gate.notified().await;
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        observation_content: None,
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
                        observation_content: None,
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
                MockTurn::ToolCalls(tool_calls) => Ok(transport::TurnOutput {
                    observation_content: None,
                    text: String::new(),
                    thinking: String::new(),
                    tool_calls,
                    stop: StopReason::ToolCalls,
                    end_turn: None,
                    usage: Usage::default(),
                }),
                MockTurn::FinalResult => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        observation_content: None,
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
                        observation_content: None,
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
                MockTurn::NativeSearch(content) => Ok(transport::TurnOutput {
                    observation_content: Some(content),
                    text: "Search completed.".into(),
                    thinking: String::new(),
                    tool_calls: vec![],
                    stop: StopReason::Done,
                    end_turn: None,
                    usage: Usage::default(),
                }),
                MockTurn::Text(t) => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        observation_content: None,
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
                        observation_content: None,
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
            json!(*self.shared.pushed_users.lock().unwrap())
        }
        fn restore(&mut self, snapshot: Value) {
            *self.shared.pushed_users.lock().unwrap() = serde_json::from_value(snapshot).unwrap();
        }
        async fn compact(
            &mut self,
            _params: transport::CompactionParams,
            _instruction: &str,
            _tools: &[transport::ToolSpec],
            _opts: &TurnOpts,
        ) -> Result<Option<String>> {
            self.shared.compact_calls.fetch_add(1, Ordering::SeqCst);
            if self.shared.compact_block.load(Ordering::SeqCst) {
                self.shared.compact_gate.notified().await;
            }
            anyhow::ensure!(
                !self.shared.compact_fail.load(Ordering::SeqCst),
                "synthetic compaction failure"
            );
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

        async fn call(&self, _input: Value, cx: &ToolCx) -> bro_tools::ToolResult {
            self.shared.tool_started.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                _ = self.shared.tool_gate.notified() => bro_tools::ToolResult::Text("slow done".into()),
                _ = cx.cancellation.cancelled() => bro_tools::ToolResult::Error(INTERRUPTED_TOOL_RESULT.into()),
            }
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

    struct DispatchProbe {
        read_only: bool,
        value: Arc<AtomicUsize>,
        finished_read: Arc<Notify>,
        pending_read: Arc<Notify>,
    }

    #[async_trait]
    impl bro_tools::Tool for DispatchProbe {
        fn name(&self) -> &str {
            if self.read_only {
                "dispatch_read"
            } else {
                "dispatch_write"
            }
        }

        fn description(&self) -> &str {
            "Test dispatch ordering and interrupted read delivery"
        }

        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }

        async fn call(&self, input: Value, cx: &ToolCx) -> bro_tools::ToolResult {
            if input["block"] == true {
                self.finished_read.notified().await;
                self.pending_read.notify_one();
                cx.cancellation.cancelled().await;
                return bro_tools::ToolResult::Error(INTERRUPTED_TOOL_RESULT.into());
            }
            let value = if self.read_only {
                self.value.load(Ordering::SeqCst)
            } else {
                self.value.fetch_add(1, Ordering::SeqCst) + 1
            };
            if self.read_only {
                self.finished_read.notify_one();
            }
            bro_tools::ToolResult::Text(value.to_string())
        }

        fn annotations(&self) -> bro_tools::ToolAnnotations {
            bro_tools::ToolAnnotations {
                read_only: self.read_only,
                destructive: false,
            }
        }
    }

    fn install_dispatch_probes(session: &mut Session) -> Arc<Notify> {
        let value = Arc::new(AtomicUsize::new(0));
        let finished_read = Arc::new(Notify::new());
        let pending_read = Arc::new(Notify::new());
        let tools: Vec<Arc<dyn bro_tools::Tool>> = [true, false]
            .into_iter()
            .map(|read_only| {
                Arc::new(DispatchProbe {
                    read_only,
                    value: value.clone(),
                    finished_read: finished_read.clone(),
                    pending_read: pending_read.clone(),
                }) as Arc<dyn bro_tools::Tool>
            })
            .collect();
        session.reg = Registry::new(
            tools,
            vec![],
            &PinPolicy::from_env(),
            &mcp::ToolFilter::default(),
        )
        .unwrap();
        pending_read
    }

    fn dispatch_call(id: &str, name: &str, args: Value) -> transport::ToolCall {
        transport::ToolCall {
            id: id.into(),
            name: name.into(),
            args,
        }
    }

    #[tokio::test]
    async fn shell_page_survives_large_result_hook_without_losing_consumed_bytes() {
        struct LargeHook;
        impl crate::hooks::Hook for LargeHook {
            fn on_tool_result(
                &self,
                _: &transport::ToolCall,
                _: &transport::ToolResult,
            ) -> Vec<crate::hooks::Candidate> {
                vec![crate::hooks::Candidate {
                    rule_id: "shell-context-fixture".into(),
                    message: "context fixture ".repeat(1000),
                    delivery: Delivery::Rider,
                    kind: crate::hooks::NudgeKind::Signpost,
                    priority: 100,
                }]
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut session, shared) = mk_session(vec![
            MockTurn::ToolCalls(vec![dispatch_call(
                "shell-fixture",
                "shell_run",
                json!({"command":"printf '%0500d' 0", "yield_time_ms":0}),
            )]),
            MockTurn::Text("done".into()),
        ]);
        session.cx.root = root;
        session.cx.output_budget = 1024;
        session.tool_result_cap = 1024;
        session.hooks =
            crate::hooks::HookEngine::new(vec![Box::new(LargeHook)], Default::default());
        session.reg = Registry::new(
            vec![Arc::new(bro_tools::ShellRun)],
            vec![],
            &PinPolicy::from_env(),
            &mcp::ToolFilter::default(),
        )
        .unwrap();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        session.emitter = Emitter::with_callback(
            "fixture".into(),
            Arc::new(move |event| {
                captured.lock().unwrap().push(event);
            }),
        );
        let (_cancel_tx, cancel) = watch::channel(false);
        session
            .user_turn("run fixture", cancel, Arc::new(Mutex::new(VecDeque::new())))
            .await
            .unwrap();
        let batches = shared.pushed_tool_results.lock().unwrap();
        let page: Value =
            serde_json::from_str(&batches[0][0].content).expect("hook cannot corrupt shell JSON");
        assert_eq!(page["stdout"], "0".repeat(500));
        assert!(batches[0][0].content.len() <= 1024);
        drop(batches);
        assert!(
            shared
                .pushed_users
                .lock()
                .unwrap()
                .iter()
                .any(|text| text.contains("context fixture"))
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event["subtype"] == "tool_result_context")
        );
        bro_tools::shell::shutdown_shell_sessions(&session.cx).await;
    }

    #[tokio::test]
    async fn yielded_cell_cannot_borrow_instruction_delivery_from_new_model_boundary() {
        use bro_tools::ToolResult;
        use std::time::Duration;

        struct Pause {
            started: tokio::sync::Semaphore,
            release: tokio::sync::Semaphore,
        }
        #[async_trait]
        impl Tool for Pause {
            fn name(&self) -> &str {
                "pause"
            }
            fn description(&self) -> &str {
                "Controlled test boundary"
            }
            fn input_schema(&self) -> Value {
                json!({"type":"object"})
            }
            async fn call(&self, _: Value, _: &ToolCx) -> ToolResult {
                self.started.add_permits(1);
                self.release.acquire().await.unwrap().forget();
                ToolResult::Json(json!({}))
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join("child")).unwrap();
        std::fs::write(root.join("child/AGENTS.md"), "EXACT CHILD INSTRUCTIONS\n").unwrap();
        std::fs::write(root.join("child/value.txt"), "before").unwrap();
        let (mut session, shared) = mk_session(vec![]);
        session.cx.root = root.clone();
        session.scoped_project_docs = Arc::new(crate::project_doc::ScopedProjectDocs::new(
            root.clone(),
            None,
        ));
        session.cx.instruction_policy = Some(session.scoped_project_docs.clone());
        session.deliver_instruction_context().await.unwrap();
        let old_generation = session.cx.instruction_generation;
        let pause = Arc::new(Pause {
            started: tokio::sync::Semaphore::new(0),
            release: tokio::sync::Semaphore::new(0),
        });
        let callable: Vec<Arc<dyn Tool>> = vec![
            Arc::new(bro_tools::file_read::FileRead),
            Arc::new(bro_tools::workspace::FileWrite),
            pause.clone(),
        ];
        let runtime = crate::code_mode::CodeModeToolSession::new(
            &callable,
            Arc::new(crate::capabilities::HostTools::new(
                callable.clone(),
                session.cx.clone(),
            )),
            crate::code_mode::CodeMode::Only,
            &BTreeMap::new(),
        );
        let tools = runtime.tools();
        let yielded = tools[0].call(
            json!({"source":"// @exec: {\"yield_time_ms\": 1}\nawait tools.file_read({file_path:'child/value.txt'}); await tools.pause({}); try { await tools.file_write({file_path:'child/value.txt',content:'stale write'}); } catch (error) { text(String(error)); }"}),
            &session.cx,
        ).await.into_content().0;
        let cell_id = yielded
            .split("Script running with cell ID ")
            .nth(1)
            .unwrap()
            .split('.')
            .next()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), pause.started.acquire())
            .await
            .unwrap()
            .unwrap()
            .forget();
        session.deliver_instruction_context().await.unwrap();
        assert!(session.cx.instruction_generation > old_generation);
        assert!(
            shared
                .pushed_users
                .lock()
                .unwrap()
                .iter()
                .any(|text| text.contains("EXACT CHILD INSTRUCTIONS\n"))
        );
        pause.release.add_permits(1);
        let result = tools[1]
            .call(json!({"cell_id":cell_id,"yield_time_ms":1000}), &session.cx)
            .await
            .into_content()
            .0;
        assert!(result.contains("instructions_required"), "{result}");
        assert_eq!(
            std::fs::read_to_string(root.join("child/value.txt")).unwrap(),
            "before"
        );
        let result = tools[0].call(
            json!({"source":"text(await tools.file_write({file_path:'child/value.txt',content:'fresh write'}));"}),
            &session.cx,
        ).await;
        assert!(!result.is_error(), "{result:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("child/value.txt")).unwrap(),
            "fresh write"
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn persistence_marks_retained_cells_and_unread_shell_output_until_drained() {
        use bro_tools::ToolResult;

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let (mut session, _) = mk_session_with_store(
            vec![],
            Some(SessionStore::for_test(root.join("checkpoint.json"))),
        );
        session.cx.root = root;
        let read_marker = |session: &Session| {
            let saved: Value =
                serde_json::from_slice(&std::fs::read(session.store.store_path()).unwrap())
                    .unwrap();
            saved["side"]["runtime_work_outstanding"].as_bool().unwrap()
        };
        let code_mode = crate::code_mode::CodeModeToolSession::new(
            &[],
            Arc::new(crate::capabilities::HostTools::new(
                vec![],
                session.cx.clone(),
            )),
            crate::code_mode::CodeMode::Only,
            &BTreeMap::new(),
        );
        let result = code_mode.tools()[0].call(
            json!({"source":"// @exec: {\"yield_time_ms\": 1}\nawait new Promise(resolve => setTimeout(resolve, 30000));"}),
            &session.cx,
        ).await;
        assert!(
            result
                .into_content()
                .0
                .contains("Script running with cell ID")
        );
        session.code_mode_session = Some(code_mode);
        session.persist().await.unwrap();
        assert!(read_marker(&session));
        session.drain_cancelled_work().await;
        session.persist().await.unwrap();
        assert!(!read_marker(&session));

        session.cx.cancellation = Default::default();
        let code_tools = session.code_mode_session.as_ref().unwrap().tools();
        for source in [
            "text('normal receipt');",
            "yield_control(); text('normal receipt');",
        ] {
            let result = code_tools[0]
                .call(json!({"source":source}), &session.cx)
                .await;
            assert!(!result.is_error(), "{result:?}");
            let content = result.into_content().0;
            if let Some(rest) = content.split("Script running with cell ID ").nth(1) {
                let cell_id = rest.split('.').next().unwrap();
                session.persist().await.unwrap();
                assert!(read_marker(&session));
                let terminal = code_tools[1]
                    .call(json!({"cell_id":cell_id,"yield_time_ms":1000}), &session.cx)
                    .await;
                assert!(!terminal.is_error(), "{terminal:?}");
                assert!(terminal.into_content().0.contains("normal receipt"));
            } else {
                assert!(content.contains("normal receipt"), "{content}");
            }
            // No scheduling grace period: normal exec and wait replies both
            // promise that terminal cleanup is complete before this capture.
            session.persist().await.unwrap();
            assert!(!read_marker(&session));
        }

        let result = bro_tools::ShellRun.call(
            json!({"command":"printf 'retained shell receipt'", "yield_time_ms":1000, "max_output_tokens":0}),
            &session.cx,
        ).await;
        let ToolResult::Json(result) = result else {
            panic!("expected shell receipt: {result:?}");
        };
        assert_eq!(result["running"], false);
        assert_eq!(result["output_pending"], true);
        session.persist().await.unwrap();
        assert!(read_marker(&session));
        let result = bro_tools::ShellPoll
            .call(json!({"session_id":result["session_id"]}), &session.cx)
            .await;
        assert!(!result.is_error(), "{result:?}");
        assert!(session.cx.shell_sessions.lock().unwrap().is_empty());
        session.persist().await.unwrap();
        assert!(!read_marker(&session));
    }

    fn mk_session(scripts: Vec<MockTurn>) -> (Session, MockShared) {
        mk_session_with_store(scripts, None)
    }

    fn mk_session_with_store(
        scripts: Vec<MockTurn>,
        store: Option<SessionStore>,
    ) -> (Session, MockShared) {
        let shared = MockShared::default();
        let mock = MockTransport {
            shared: shared.clone(),
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
        };
        let todos = Arc::new(Mutex::new(bro_tools::TodoList::default()));
        let cx = ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root: std::env::temp_dir(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            output_budget: 16 * 1024,
            child_env: Arc::new(Default::default()),
            cancellation: Default::default(),
            session_env: Arc::new(BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        };
        let session = Session {
            tx: Box::new(mock),
            code_mode: crate::code_mode::CodeMode::Optional,
            code_mode_session: None,
            remote_outcome_sources: Vec::new(),
            retain_background_work: true,
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
            )
            .unwrap(),
            cx,
            reference_context_item: None,
            hooks: HookEngine::from_env(NudgeLedger::from_side(&Value::Null)),
            scoped_project_docs: Arc::new(crate::project_doc::ScopedProjectDocs::default()),
            resume_runtime_reset: false,
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
            instruction_system: None,
            max_turns: 50,
            compaction: crate::compaction::CompactionPolicy::from_env(),
            compact_threshold: None,
            context_window: None,
            tool_result_cap: 0,
            store: store.unwrap_or_else(SessionStore::temporary_for_test),
            event_log: Arc::new(EventLog::disabled()),
            seq_counter: Arc::new(AtomicU64::new(0)),
            prior_side: Value::Null,
            pending_user_inputs: VecDeque::new(),
            todos,
            lsp_baselines: LspBaselines::default(),
            lsp_pool: bro_lsp::SessionPool::new(bro_lsp::LspConfig::default()),
            lsp_documents: BTreeMap::new(),
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
            pending_input_estimate: 0,
            last_request_overhead_tokens: 0,
            tail_nudge: None,
        };
        (session, shared)
    }

    async fn startup_docs_at(root: &std::path::Path) -> crate::project_doc::ScopedProjectDocs {
        transport::with_session_env(
            BTreeMap::from([
                (
                    "CODEX_HOME".into(),
                    root.join("codex-home").display().to_string(),
                ),
                (
                    "BRO_HARNESS_PROJECT_DOC_FILES".into(),
                    "AGENTS.override.md,AGENTS.md".into(),
                ),
            ]),
            async { crate::project_doc::ScopedProjectDocs::for_session(root.to_path_buf(), None) },
        )
        .await
    }

    async fn install_startup_instructions(session: &mut Session, body: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        std::fs::create_dir(root.join("codex-home")).unwrap();
        std::fs::write(root.join("AGENTS.md"), body).unwrap();
        let docs = startup_docs_at(&root).await;
        session.cx.root = root;
        session.scoped_project_docs = Arc::new(docs);
        session.cx.instruction_policy = Some(session.scoped_project_docs.clone());
        directory
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
    async fn final_result_rejects_invalid_and_sibling_calls_before_dispatch() {
        for calls in [
            vec![dispatch_call(
                "bad",
                FINAL_RESULT_TOOL,
                json!({"ok":"wrong"}),
            )],
            vec![
                dispatch_call("one", FINAL_RESULT_TOOL, json!({"ok":true})),
                dispatch_call("two", FINAL_RESULT_TOOL, json!({"ok":true})),
            ],
            vec![
                dispatch_call("mutation", "slow_tool", json!({})),
                dispatch_call("final", FINAL_RESULT_TOOL, json!({"ok":true})),
            ],
        ] {
            let count = calls.len();
            let (mut session, shared) =
                mk_session(vec![MockTurn::ToolCalls(calls), MockTurn::FinalResult]);
            session.output_schema = Some(
                json!({"type":"object", "properties":{"ok":{"type":"boolean"}}, "required":["ok"]}),
            );
            shared.tool_gate.notify_one();
            run_user_turn(&mut session, "structured result").await;
            assert_eq!(shared.tool_started.load(Ordering::SeqCst), 0);
            let results = shared.pushed_tool_results.lock().unwrap();
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].len(), count);
            assert!(results[0].iter().all(|result| result.is_error));
            assert!(!results[1][0].is_error);
        }
    }

    #[tokio::test]
    async fn mechanical_incomplete_stops_never_emit_success_or_dispatch_calls() {
        for (scripts, limit, schema, reason) in [
            (
                vec![MockTurn::Terminal(
                    StopReason::Length,
                    "partial".into(),
                    vec![dispatch_call("uncommitted", "slow_tool", json!({}))],
                )],
                50,
                false,
                "output_limit",
            ),
            (
                vec![MockTurn::TextWithEndTurn(
                    "still working".into(),
                    Some(false),
                )],
                1,
                false,
                "max_turns",
            ),
            (
                vec![MockTurn::Text("plain text".into())],
                50,
                true,
                "model_stop",
            ),
            (
                vec![MockTurn::Text("".into()), MockTurn::Text("".into())],
                50,
                false,
                "model_stop",
            ),
        ] {
            let (mut session, shared) = mk_session(scripts);
            session.max_turns = limit;
            if schema {
                session.output_schema = Some(json!({"type":"object"}));
            }
            let events = Arc::new(Mutex::new(Vec::<Value>::new()));
            let captured = events.clone();
            session.emitter = Emitter::with_callback(
                "terminal-fixture".into(),
                Arc::new(move |event| captured.lock().unwrap().push(event)),
            );
            shared.tool_gate.notify_one();
            run_user_turn(&mut session, "finish").await;
            assert_eq!(shared.tool_started.load(Ordering::SeqCst), 0);
            let events = events.lock().unwrap();
            let result = events
                .iter()
                .rev()
                .find(|event| event["type"] == "result")
                .unwrap();
            assert_eq!(result["subtype"], "incomplete");
            assert_eq!(result["is_error"], true);
            assert_eq!(result["stop_reason"], reason);
        }
    }

    #[tokio::test]
    async fn scoped_read_preserves_source_and_delivers_instructions_at_next_request() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let child = root.join("crates/thing");
        std::fs::create_dir_all(child.join("src")).unwrap();
        std::fs::write(child.join("AGENTS.md"), "CHILD-DOC").unwrap();
        std::fs::write(child.join("src/lib.rs"), "FILE-BODY").unwrap();
        let (mut session, shared) = mk_session(vec![
            MockTurn::FileReadUnderChild,
            MockTurn::Text("done".into()),
        ]);
        session.cx.root = root.clone();
        session.scoped_project_docs =
            Arc::new(crate::project_doc::ScopedProjectDocs::new(root, None));
        session.cx.instruction_policy = Some(session.scoped_project_docs.clone());
        session.reg = Registry::new(
            bro_tools::builtin_tools(),
            vec![],
            &PinPolicy::default(),
            &crate::mcp::ToolFilter::default(),
        )
        .unwrap();
        run_user_turn(&mut session, "read it").await;
        let pushed = shared.pushed_tool_results.lock().unwrap();
        assert_eq!(pushed.len(), 1);
        assert!(pushed[0][0].content.contains("FILE-BODY"));
        assert!(!pushed[0][0].content.contains("CHILD-DOC"));
        let users = shared.pushed_users.lock().unwrap();
        assert_eq!(
            users
                .iter()
                .filter(|text| text.contains("CHILD-DOC"))
                .count(),
            1
        );
        assert!(session.scoped_project_docs.pending_batch(99).is_none());
    }

    #[tokio::test]
    async fn resumed_missing_schemas_stop_before_model_or_compaction() {
        let cases = [
            (
                "policy_denied",
                Some(json!(["file_read"])),
                true,
                true,
                true,
                false,
            ),
            (
                "catalog_missing",
                Some(json!(["file_read"])),
                false,
                false,
                false,
                false,
            ),
            (
                "explicit_empty_receipt",
                Some(json!([])),
                true,
                false,
                true,
                false,
            ),
            (
                "explicit_empty_snapshot_receipt",
                Some(json!([])),
                true,
                false,
                false,
                false,
            ),
            (
                "intact",
                Some(json!(["file_read"])),
                true,
                false,
                true,
                true,
            ),
            (
                "intact_saved_only",
                Some(json!(["file_read"])),
                true,
                false,
                false,
                true,
            ),
            ("legacy_snapshot_receipt", None, true, false, false, true),
            ("legacy_log_receipt", None, true, false, true, true),
            (
                "unactivated_code_mode",
                Some(json!([])),
                true,
                false,
                false,
                true,
            ),
        ];
        for (case, saved, present, denied, log_receipt, succeeds) in cases {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let log = Arc::new(EventLog::at_path(root.join("events.jsonl")));
            let call = json!({"role":"assistant","content":[{
                "type":"tool_use","id":"load-read","name":"tool_search","input":{"query":"select:file_read"}
            }]});
            let result = json!({"role":"user","content":[{
                "type":"tool_result","tool_use_id":"load-read","is_error":false,
                "content":json!({"loaded":["file_read"]}).to_string()
            }]});
            if log_receipt {
                log.append_event(&json!({"type":"assistant","message":call}));
                log.append_event(&json!({"type":"user","message":result}));
                let flush = log.clone();
                tokio::task::spawn_blocking(move || flush.flush_blocking())
                    .await
                    .unwrap();
            }
            let receipts = if case.ends_with("snapshot_receipt") {
                EventLog::snapshot_tool_search_activations(&json!([call, result]))
            } else {
                let path = log.path().to_path_buf();
                tokio::task::spawn_blocking(move || EventLog::tool_search_activations(&path))
                    .await
                    .unwrap()
            };
            let (mut session, shared) = mk_session_with_store(
                vec![MockTurn::Text("checkpoint".into())],
                Some(SessionStore::for_test(root.join("session.json"))),
            );
            let builtins: Vec<Arc<dyn Tool>> = if present {
                vec![Arc::new(FileReadTool)]
            } else {
                vec![]
            };
            let filter = mcp::ToolFilter::from_csv(denied.then_some("file_read"), None);
            session.reg =
                Registry::with_options(builtins, vec![], &PinPolicy::default(), &filter, true)
                    .unwrap();
            session
                .reg
                .restore_resume_activations(saved.as_ref(), &receipts);
            session.event_log = log.clone();
            session.emitter = Emitter::new(case.into()).with_event_log(log.clone());
            session.cx.root = root;
            // Force the proactive compaction branch if the guard were late.
            session.compact_threshold = Some(0);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let result = session
                .user_turn(
                    "Report the checkpoint.",
                    cancel_rx,
                    Arc::new(StdMutex::new(VecDeque::new())),
                )
                .await;
            assert_eq!(result.is_ok(), succeeds, "{case}");
            if succeeds {
                assert_eq!(shared.started.load(Ordering::SeqCst), 1, "{case}");
                if case == "unactivated_code_mode" {
                    assert_eq!(session.reg.activation_state(), json!([]));
                    assert!(
                        !session
                            .reg
                            .wire_specs()
                            .iter()
                            .any(|tool| tool.name == "file_read")
                    );
                } else {
                    assert_eq!(session.reg.activation_state(), json!(["file_read"]));
                }
            } else {
                let error = result.unwrap_err().to_string();
                assert!(
                    error.contains("error.resume_tool_schema_missing"),
                    "{case}: {error}"
                );
                assert!(error.contains("file_read"), "{case}: {error}");
                assert!(session.compact_manual().await.is_err(), "{case}");
                assert_eq!(shared.started.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 0, "{case}");
                assert_eq!(session.reg.activation_state(), json!([]), "{case}");
                let rows: Vec<Value> = std::fs::read_to_string(log.path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
                let terminal: Vec<_> = rows
                    .iter()
                    .filter(|row| row["event"]["type"] == "result")
                    .collect();
                assert_eq!(terminal.len(), 1, "{case}");
                assert_eq!(terminal[0]["event"]["is_error"], true, "{case}");
                assert!(
                    terminal[0]["event"]["result"]
                        .as_str()
                        .unwrap()
                        .contains("error.resume_tool_schema_missing"),
                    "{case}"
                );
            }
        }
    }

    struct CountingAction(Arc<AtomicUsize>);

    #[async_trait]
    impl bro_tools::Tool for CountingAction {
        fn name(&self) -> &str {
            "count_action"
        }
        fn description(&self) -> &str {
            "Count synthetic actions"
        }
        fn input_schema(&self) -> Value {
            json!({"type":"object"})
        }
        async fn call(&self, _input: Value, _cx: &ToolCx) -> bro_tools::ToolResult {
            self.0.fetch_add(1, Ordering::SeqCst);
            bro_tools::ToolResult::Text("executed".into())
        }
    }

    fn ambiguous_native_batch(suffix: &str) -> Vec<Value> {
        vec![
            json!({"type":"server_tool_use","id":format!("native-{suffix}"),"name":"web_search_prime","input":{"search_query":"synthetic"}}),
            json!({"type":"tool_use","id":format!("early-{suffix}"),"name":"count_action","input":{"value":1}}),
            json!({"type":"tool_result","tool_use_id":format!("native-{suffix}"),"content":"[]"}),
            json!({"type":"tool_use","id":format!("late-{suffix}"),"name":"count_action","input":{"value":1}}),
            json!({"type":"tool_use","id":format!("sibling-{suffix}"),"name":"count_action","input":{"value":2}}),
            json!({"type":"tool_use","id":format!("final-{suffix}"),"name":"final_result","input":{"done":true}}),
        ]
    }

    fn complete_block_sse(blocks: &[Value]) -> String {
        let mut events = vec![
            json!({"type":"message_start","message":{"id":"synthetic","role":"assistant","content":[]}}),
        ];
        for (index, block) in blocks.iter().enumerate() {
            if block["type"] == "text" {
                events.push(json!({"type":"content_block_start","index":index,"content_block":{"type":"text","text":""}}));
                events.push(json!({"type":"content_block_delta","index":index,"delta":{"type":"text_delta","text":block["text"]}}));
            } else {
                events.push(
                    json!({"type":"content_block_start","index":index,"content_block":block}),
                );
            }
            events.push(json!({"type":"content_block_stop","index":index}));
        }
        let stop = if blocks.iter().any(|b| b["type"] == "tool_use") {
            "tool_use"
        } else {
            "end_turn"
        };
        events.push(json!({"type":"message_delta","delta":{"stop_reason":stop}}));
        events.push(json!({"type":"message_stop"}));
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[tokio::test]
    async fn glm_ambiguous_batches_reject_all_calls_and_bound_correction() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let corrected = vec![
            json!({"type":"tool_use","id":"corrected","name":"count_action","input":{"value":1}}),
        ];
        let done = vec![json!({"type":"text","text":"done"})];
        let ordinary = vec![
            json!({"type":"tool_use","id":"repeat-a","name":"count_action","input":{"value":1}}),
            json!({"type":"tool_use","id":"repeat-b","name":"count_action","input":{"value":1}}),
        ];
        for (label, responses, max_turns, user_turns, expected_counts, expected_error) in [
            (
                "corrected",
                vec![ambiguous_native_batch("a"), corrected.clone(), done.clone()],
                10,
                1,
                vec![0, 0, 1],
                None,
            ),
            (
                "serial-correction-repeats",
                vec![
                    ambiguous_native_batch("a"),
                    corrected.clone(),
                    vec![
                        json!({"type":"tool_use","id":"corrected-again","name":"count_action","input":{"value":1}}),
                    ],
                    done.clone(),
                ],
                10,
                1,
                vec![0, 0, 1, 2],
                None,
            ),
            (
                "recurrent",
                vec![ambiguous_native_batch("a"), ambiguous_native_batch("b")],
                10,
                1,
                vec![0, 0],
                Some("Automatic correction was already attempted"),
            ),
            (
                "multi-call-correction",
                vec![ambiguous_native_batch("a"), ordinary.clone()],
                10,
                1,
                vec![0, 0],
                Some("Correction protocol violation"),
            ),
            (
                "distinct-call-correction",
                vec![
                    ambiguous_native_batch("a"),
                    vec![
                        corrected[0].clone(),
                        json!({"type":"tool_use","id":"correction-final","name":"final_result","input":{"done":true}}),
                    ],
                ],
                10,
                1,
                vec![0, 0],
                Some("Correction protocol violation"),
            ),
            (
                "no-budget",
                vec![ambiguous_native_batch("a")],
                1,
                1,
                vec![0],
                Some("No correction turn remains"),
            ),
            (
                "ordinary",
                vec![ordinary, done.clone()],
                10,
                1,
                vec![0, 2],
                None,
            ),
            (
                "separate-turns",
                vec![corrected.clone(), done.clone(), corrected.clone(), done],
                10,
                2,
                vec![0, 1, 1, 2],
                None,
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let count = Arc::new(AtomicUsize::new(0));
            let (mut session, _) = mk_session_with_store(
                vec![],
                Some(SessionStore::for_test(root.join("session.json"))),
            );
            session.cx.root = root.clone();
            session.max_turns = max_turns;
            let schema = json!({"type":"object","properties":{"done":{"type":"boolean"}}});
            // An ambiguous final_result sibling must not terminate the turn.
            if label != "ordinary" && label != "separate-turns" {
                session.output_schema = Some(schema.clone());
            }
            session.reg = Registry::new(
                vec![
                    Arc::new(CountingAction(count.clone())),
                    Arc::new(FinalResultTool::new(schema)),
                ],
                vec![],
                &PinPolicy::default(),
                &mcp::ToolFilter::default(),
            )
            .unwrap();
            let log = Arc::new(EventLog::at_path(root.join("events.jsonl")));
            session.event_log = log.clone();
            session.emitter = Emitter::new(label.into()).with_event_log(log.clone());
            session.base_opts.model = "glm-5.3".into();
            session.base_opts.web_search = true;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server_count = count.clone();
            let expected_responses = responses.clone();
            let server = tokio::spawn(async move {
                let mut requests = Vec::new();
                for blocks in responses {
                    let (mut socket, _) = listener.accept().await.unwrap();
                    let mut request = Vec::new();
                    let body = loop {
                        let mut chunk = [0_u8; 4096];
                        let read = socket.read(&mut chunk).await.unwrap();
                        assert_ne!(read, 0);
                        request.extend_from_slice(&chunk[..read]);
                        if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let headers = String::from_utf8_lossy(&request[..header_end]);
                            let length = headers
                                .lines()
                                .filter_map(|line| line.split_once(':'))
                                .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                                .unwrap();
                            if request.len() >= header_end + 4 + length {
                                break serde_json::from_slice::<Value>(
                                    &request[header_end + 4..header_end + 4 + length],
                                )
                                .unwrap();
                            }
                        }
                    };
                    requests.push((body, server_count.load(Ordering::SeqCst)));
                    let body = complete_block_sse(&blocks);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                }
                requests
            });
            session.tx = Box::new(
                transport::with_session_env(
                    BTreeMap::from([
                        ("ANTHROPIC_BASE_URL".into(), format!("http://{addr}")),
                        ("ANTHROPIC_AUTH_TOKEN".into(), "synthetic-token".into()),
                        ("BRO_HARNESS_PROVIDER".into(), "glm".into()),
                    ]),
                    async { transport::anthropic::AnthropicTransport::from_env().unwrap() },
                )
                .await,
            );
            for _ in 0..user_turns {
                let (_cancel_tx, cancel_rx) = watch::channel(false);
                let result = tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    session.user_turn(
                        "Perform the intended synthetic action.",
                        cancel_rx,
                        Arc::new(StdMutex::new(VecDeque::new())),
                    ),
                )
                .await
                .expect(label);
                if let Some(message) = expected_error {
                    let error = result.expect_err(label).to_string();
                    assert!(
                        error.contains("error.glm_ambiguous_client_batch"),
                        "{error}"
                    );
                    assert!(error.contains(message), "{error}");
                } else {
                    result.unwrap();
                }
            }
            let requests = server.await.unwrap();
            assert_eq!(
                requests.iter().map(|(_, count)| *count).collect::<Vec<_>>(),
                expected_counts,
                "{label}"
            );
            assert_eq!(
                count.load(Ordering::SeqCst),
                *expected_counts.last().unwrap(),
                "{label}"
            );
            // Wire requests add rolling cache breakpoints to recent blocks;
            // this metadata is intentionally absent from durable replay state.
            let replay_messages: Vec<Vec<Value>> = requests
                .iter()
                .map(|(request, _)| {
                    let mut messages = request["messages"].as_array().unwrap().clone();
                    for message in &mut messages {
                        for block in message["content"].as_array_mut().unwrap() {
                            block.as_object_mut().unwrap().remove("cache_control");
                        }
                    }
                    messages
                })
                .collect();
            let snapshot = session.tx.snapshot();
            let messages = snapshot.as_array().unwrap();
            for blocks in expected_responses.iter().filter(|blocks| {
                blocks.iter().any(|b| b["type"] == "server_tool_use")
                    || (expected_error.is_some() && blocks.iter().any(|b| b["type"] == "tool_use"))
            }) {
                let pos = messages
                    .iter()
                    .position(|message| {
                        message["role"] == "assistant" && message["content"] == json!(blocks)
                    })
                    .expect("exact original native/client blocks retained");
                let errors = messages[pos + 1]["content"].as_array().unwrap();
                let clients: Vec<_> = blocks
                    .iter()
                    .filter(|block| block["type"] == "tool_use")
                    .collect();
                assert_eq!(errors.len(), clients.len());
                for (error, client) in errors.iter().zip(clients) {
                    assert_eq!(error["tool_use_id"], client["id"]);
                    assert_eq!(error["is_error"], true);
                    assert!(
                        error["content"]
                            .as_str()
                            .unwrap()
                            .contains("None of the client tools in this batch were executed")
                    );
                }
                if blocks == &expected_responses[0] && label != "no-budget" {
                    let correction = &replay_messages[1];
                    assert!(
                        correction.contains(&messages[pos]),
                        "immediate correction must replay the original response"
                    );
                    assert!(
                        correction.contains(&messages[pos + 1]),
                        "immediate correction must replay every paired rejection"
                    );
                }
                for replay in replay_messages
                    .iter()
                    .filter(|replay| replay.contains(&messages[pos]))
                {
                    assert!(replay.contains(&messages[pos + 1]));
                }
            }
            let rows: Vec<Value> = std::fs::read_to_string(log.path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let terminals: Vec<_> = rows
                .iter()
                .filter(|r| r["event"]["type"] == "result")
                .collect();
            assert_eq!(terminals.len(), user_turns);
            assert_eq!(
                terminals.last().unwrap()["event"]["is_error"]
                    .as_bool()
                    .unwrap_or(false),
                expected_error.is_some()
                    || (session.output_schema.is_some()
                        && !expected_responses
                            .last()
                            .unwrap()
                            .iter()
                            .any(|block| block["name"] == FINAL_RESULT_TOOL)),
                "{label}: a schema session without final_result is incomplete"
            );
            for message in messages.iter().filter(|m| m["role"] == "assistant") {
                assert!(rows.iter().any(|r| r["event"]["type"] == "assistant"
                    && r["event"]["message"]["content"] == message["content"]));
            }
        }
    }

    #[tokio::test]
    async fn failed_native_search_response_is_durable_without_dispatch_or_replay() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for malformed_client in [true, false] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let log = Arc::new(EventLog::at_path(root.join("events.jsonl")));
            let (mut session, shared) = mk_session_with_store(
                vec![],
                Some(SessionStore::for_test(root.join("session.json"))),
            );
            session.cx.root = root;
            session.event_log = log.clone();
            session.emitter = Emitter::new("failed-native-test".into()).with_event_log(log.clone());
            session.base_opts.model = "glm-5.3".into();
            session.base_opts.web_search = true;

            let mut events = vec![json!({"type":"message_start","message":{
                "id":"failed-message","role":"assistant","content":[]
            }})];
            let mut expected_native = Vec::new();
            for (number, index) in [1, 5, 9, 13, 19].into_iter().enumerate() {
                let id = format!("native-{number}");
                let call = json!({"type":"server_tool_use","id":id,
                    "name":"web_search_prime","input":{"search_query":"placeholder"}});
                let result = json!({"type":"tool_result","tool_use_id":id,
                    "content":"[[{\"title\":\"Synthetic result\",\"link\":\"https://example.com\"}]]"});
                events
                    .push(json!({"type":"content_block_start","index":index,"content_block":call}));
                events.push(json!({"type":"content_block_stop","index":index}));
                events.push(
                    json!({"type":"content_block_start","index":index+2,"content_block":result}),
                );
                events.push(json!({"type":"content_block_stop","index":index+2}));
                expected_native.extend([call, result]);
            }
            if malformed_client {
                // The provider starts a client call, truncates its arguments,
                // then reuses that ID for a differently named native call.
                events.push(
                    json!({"type":"content_block_start","index":23,"content_block":{
                        "type":"tool_use","id":"duplicate-id","name":"tool_search","input":{}
                    }}),
                );
                events.push(json!({"type":"content_block_delta","index":23,
                    "delta":{"type":"input_json_delta","partial_json":"{\""}}));
                let mislabeled = json!({"type":"server_tool_use","id":"duplicate-id",
                    "name":"web_search_prime","input":{"query":"select:mcp__blackbox__bro_report"}});
                events.push(
                    json!({"type":"content_block_start","index":24,"content_block":mislabeled}),
                );
                expected_native.push(mislabeled);
                events.push(json!({"type":"message_delta","delta":{"stop_reason":"tool_use"}}));
                events.push(json!({"type":"message_stop"}));
            } else {
                events.push(json!({"type":"error","error":{
                    "type":"overloaded_error","message":"try again"
                }}));
            }
            let body: String = events
                .iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 4096];
                    let read = socket.read(&mut chunk).await.unwrap();
                    assert_ne!(read, 0);
                    request.extend_from_slice(&chunk[..read]);
                    if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                        let headers = String::from_utf8_lossy(&request[..header_end]);
                        let length = headers
                            .lines()
                            .filter_map(|line| line.split_once(':'))
                            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
                            .map(|(_, value)| value.trim().parse::<usize>().unwrap())
                            .unwrap();
                        if request.len() >= header_end + 4 + length {
                            break;
                        }
                    }
                }
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            });
            let tx = transport::with_session_env(
                BTreeMap::from([
                    ("ANTHROPIC_BASE_URL".into(), format!("http://{addr}")),
                    ("ANTHROPIC_AUTH_TOKEN".into(), "synthetic-token".into()),
                    ("BRO_HARNESS_PROVIDER".into(), "glm".into()),
                ]),
                async { transport::anthropic::AnthropicTransport::from_env().unwrap() },
            )
            .await;
            session.tx = Box::new(tx);
            let (_cancel_tx, cancel_rx) = watch::channel(false);
            let error = tokio::time::timeout(
                std::time::Duration::from_secs(10),
                session.user_turn(
                    "Report the checkpoint without searching the web.",
                    cancel_rx,
                    Arc::new(StdMutex::new(VecDeque::new())),
                ),
            )
            .await
            .expect("failed response must terminate")
            .expect_err("malformed or interrupted native response must fail");
            assert!(
                error
                    .downcast_ref::<transport::FailedTurnObservation>()
                    .is_some()
            );
            server.await.unwrap();

            let rows: Vec<Value> = std::fs::read_to_string(log.path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let observations: Vec<_> = rows
                .iter()
                .filter(|row| row["event"]["subtype"] == "failed_turn_observation")
                .collect();
            assert_eq!(observations.len(), 1);
            let observation = &observations[0]["event"];
            assert_eq!(observation["replayable"], false);
            assert_eq!(observation["native_blocks"], json!(expected_native));
            let diagnostics: Vec<_> = observation["tool_diagnostics"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|diagnostic| diagnostic["kind"] != "rejected_provider_response")
                .collect();
            if malformed_client {
                assert_eq!(diagnostics.len(), 3);
                assert_eq!(diagnostics[0]["kind"], "incomplete_tool_input");
                assert_eq!(diagnostics[0]["input_json"], "{\"");
                assert_eq!(diagnostics[0]["client_dispatched"], false);
                assert_eq!(diagnostics[1]["kind"], "duplicate_tool_id");
                assert_eq!(diagnostics[1]["block_indices"], json!([23, 24]));
                assert_eq!(diagnostics[2]["kind"], "native_result_missing");
                assert_eq!(diagnostics[2]["id"], "duplicate-id");
                assert_eq!(diagnostics[2]["execution_status"], "unknown");
            } else {
                assert!(diagnostics.is_empty());
            }
            let results: Vec<_> = rows
                .iter()
                .filter(|row| row["event"]["type"] == "result")
                .collect();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0]["event"]["is_error"], true);
            assert!(rows.iter().all(|row| row["event"]["type"] != "assistant"));
            assert_eq!(session.turns, 0);
            assert_eq!(shared.tool_started.load(Ordering::SeqCst), 0);
            assert!(
                session
                    .reg
                    .activation_state()
                    .as_array()
                    .unwrap()
                    .is_empty()
            );
            let snapshot = session.tx.snapshot();
            assert!(
                snapshot
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|message| message["role"] == "user")
            );
            assert!(!snapshot.to_string().contains("native-"));
            assert!(!snapshot.to_string().contains("duplicate-id"));
        }
    }

    #[tokio::test]
    async fn native_search_blocks_survive_durable_events_without_client_dispatch() {
        for (name, result) in [
            (
                "web_search",
                json!({"type":"web_search_tool_result", "tool_use_id":"native-1", "content":[]}),
            ),
            (
                "web_search_prime",
                json!({"type":"tool_result", "tool_use_id":"native-1", "content":"[]"}),
            ),
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let log = Arc::new(EventLog::at_path(root.join("events.jsonl")));
            let content = vec![
                json!({"type":"text", "text":"Searching."}),
                json!({"type":"server_tool_use", "id":"native-1", "name":name, "input":{"search_query":"synthetic"}}),
                result,
                json!({"type":"text", "text":"Search completed."}),
            ];
            let (mut session, shared) = mk_session(vec![MockTurn::NativeSearch(content.clone())]);
            session.event_log = log.clone();
            session.emitter = Emitter::new("native-search-test".into()).with_event_log(log.clone());
            run_user_turn(&mut session, "synthetic search").await;
            let lines: Vec<Value> = std::fs::read_to_string(log.path())
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            let assistants: Vec<_> = lines
                .iter()
                .filter(|line| line["event"]["type"] == "assistant")
                .collect();
            assert_eq!(assistants.len(), 1);
            assert_eq!(assistants[0]["event"]["message"]["content"], json!(content));
            assert!(
                shared.pushed_tool_results.lock().unwrap().is_empty(),
                "provider-owned calls must never be dispatched again by the client"
            );
        }
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

    #[tokio::test]
    async fn reads_observe_writes_in_model_call_order() {
        let calls = vec![
            dispatch_call("before", "dispatch_read", json!({})),
            dispatch_call("write-1", "dispatch_write", json!({})),
            dispatch_call("after-1", "dispatch_read", json!({})),
            dispatch_call("after-1-again", "dispatch_read", json!({})),
            dispatch_call("write-2", "dispatch_write", json!({})),
            dispatch_call("after-2", "dispatch_read", json!({})),
        ];
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut session, shared) = mk_session_with_store(
            vec![MockTurn::ToolCalls(calls)],
            Some(SessionStore::for_test(root.join("session.json"))),
        );
        session.cx.root = root;
        install_dispatch_probes(&mut session);
        run_user_turn(&mut session, "read around each mutation").await;
        let batches = shared.pushed_tool_results.lock().unwrap();
        let observed: Vec<_> = batches[0]
            .iter()
            .map(|r| (r.id.as_str(), r.content.as_str()))
            .collect();
        assert_eq!(
            observed,
            vec![
                ("before", "0"),
                ("write-1", "1"),
                ("after-1", "1"),
                ("after-1-again", "1"),
                ("write-2", "2"),
                ("after-2", "2"),
            ]
        );
    }

    #[tokio::test]
    async fn idle_exit_interrupt_and_provider_failure_drain_retained_processes() {
        for case in [
            "idle_interrupt",
            "idle_eof",
            "until_idle",
            "provider_failure",
            "one_shot",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let scripts = if case == "provider_failure" {
                vec![MockTurn::Failure]
            } else {
                vec![MockTurn::Text("done".into())]
            };
            let (mut session, _) = mk_session_with_store(
                scripts,
                Some(SessionStore::for_test(root.join("session.json"))),
            );
            session.cx.root = root;
            let events = Arc::new(Mutex::new(Vec::<Value>::new()));
            let sink = events.clone();
            let callback: crate::emit::EventCallback =
                Arc::new(move |event| sink.lock().unwrap().push(event));
            session.emitter = Emitter::with_callback("fixture".into(), callback.clone());
            let ctrl = Emitter::with_callback("fixture".into(), callback);
            let shell = bro_tools::ShellRun
                .call(
                    json!({"command":"sleep 30", "yield_time_ms":1}),
                    &session.cx,
                )
                .await;
            assert!(!shell.is_error(), "{case}: {shell:?}");
            assert_eq!(session.cx.shell_sessions.lock().unwrap().len(), 1);
            let run = async {
                if case == "provider_failure" || case == "one_shot" {
                    session.retain_background_work = case != "one_shot";
                    let (_cancel_tx, cancel_rx) = watch::channel(false);
                    let result = session
                        .user_turn(
                            "finish",
                            cancel_rx,
                            Arc::new(StdMutex::new(VecDeque::new())),
                        )
                        .await;
                    assert_eq!(result.is_err(), case == "provider_failure");
                } else {
                    let (input_tx, input_rx) = mpsc::unbounded_channel();
                    if case == "idle_interrupt" {
                        input_tx
                            .send(Input::Control {
                                subtype: "interrupt".into(),
                                req_id: Some("stop".into()),
                                raw: json!({}),
                            })
                            .unwrap();
                    }
                    if case == "until_idle" {
                        session_loop_until_idle(&mut session, input_rx, &ctrl, VecDeque::new())
                            .await
                            .unwrap();
                    } else {
                        drop(input_tx);
                        session_loop(&mut session, input_rx, &ctrl, VecDeque::new())
                            .await
                            .unwrap();
                    }
                }
            };
            tokio::time::timeout(std::time::Duration::from_secs(5), run)
                .await
                .unwrap();
            assert!(
                session.cx.shell_sessions.lock().unwrap().is_empty(),
                "{case}"
            );
            let events = events.lock().unwrap();
            let observation = events
                .iter()
                .position(|event| event["subtype"] == "tool_cancellation_outcomes")
                .unwrap();
            assert!(
                events[observation].to_string().contains("cancelled"),
                "{case}"
            );
            if let Some(terminal) = events
                .iter()
                .position(|event| event["type"] == "result" || event["type"] == "control_response")
            {
                assert!(observation < terminal, "{case}");
            }
        }
    }

    #[tokio::test]
    async fn interrupt_waits_for_blocking_mutation_and_reports_committed_result() {
        struct BlockingMutation {
            started: Arc<Notify>,
            release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        }
        #[async_trait]
        impl Tool for BlockingMutation {
            fn name(&self) -> &str {
                "blocking_mutation"
            }
            fn description(&self) -> &str {
                "Synthetic committed mutation"
            }
            fn input_schema(&self) -> Value {
                json!({"type":"object"})
            }
            async fn call(&self, _: Value, cx: &ToolCx) -> bro_tools::ToolResult {
                let release = self.release.lock().unwrap().take().unwrap();
                let started = self.started.clone();
                let cx = cx.clone();
                bro_tools::tool::call_blocking(move || {
                    started.notify_one();
                    release.blocking_recv().unwrap();
                    let path = cx.root.join("committed.txt");
                    std::fs::write(&path, b"committed").unwrap();
                    cx.edits
                        .lock()
                        .unwrap()
                        .push(bro_tools::EditEvent::from_bytes(path, b"", b"committed"));
                    bro_tools::ToolResult::Text("committed mutation".into())
                })
                .await
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut session, shared) = mk_session_with_store(
            vec![MockTurn::ToolCalls(vec![
                dispatch_call("active", "blocking_mutation", json!({})),
                dispatch_call("not-started", "blocking_mutation", json!({})),
            ])],
            Some(SessionStore::for_test(root.join("session.json"))),
        );
        session.cx.root = root.clone();
        let started = Arc::new(Notify::new());
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        session.reg = Registry::new(
            vec![Arc::new(BlockingMutation {
                started: started.clone(),
                release: Mutex::new(Some(release_rx)),
            })],
            vec![],
            &PinPolicy::default(),
            &mcp::ToolFilter::default(),
        )
        .unwrap();
        let events = Arc::new(Mutex::new(Vec::<Value>::new()));
        let sink = events.clone();
        session.emitter = Emitter::with_callback(
            "fixture".into(),
            Arc::new(move |event| sink.lock().unwrap().push(event)),
        );
        let completed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let done = completed.clone();
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let run = async {
            session
                .user_turn(
                    "Mutate fixture",
                    cancel_rx,
                    Arc::new(StdMutex::new(VecDeque::new())),
                )
                .await
                .unwrap();
            done.store(true, Ordering::SeqCst);
        };
        let interrupt = async {
            started.notified().await;
            cancel_tx.send(true).unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            let acknowledged_early = completed.load(Ordering::SeqCst);
            let wrote_early = root.join("committed.txt").exists();
            release_tx.send(()).unwrap();
            assert!(
                !acknowledged_early,
                "interruption acknowledged before the mutation finished"
            );
            assert!(!wrote_early);
        };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(run, interrupt);
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read(root.join("committed.txt")).unwrap(),
            b"committed"
        );
        let batches = shared.pushed_tool_results.lock().unwrap();
        assert_eq!(batches[0][0].content, "committed mutation");
        assert!(!batches[0][0].is_error);
        assert_eq!(batches[0][1].content, INTERRUPTED_TOOL_RESULT);
        let events = events.lock().unwrap();
        let observation = events
            .iter()
            .position(|event| event["subtype"] == "tool_cancellation_outcomes")
            .unwrap();
        let terminal = events
            .iter()
            .position(|event| event["type"] == "result")
            .unwrap();
        assert!(observation < terminal);
        assert!(events[observation].to_string().contains("committed.txt"));
        assert!(session.cx.edits.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn interrupted_read_batch_retains_completed_results_and_skips_later_writes() {
        let calls = vec![
            dispatch_call("completed", "dispatch_read", json!({})),
            dispatch_call("pending", "dispatch_read", json!({"block": true})),
            dispatch_call("unstarted", "dispatch_write", json!({})),
        ];
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let (mut session, shared) = mk_session_with_store(
            vec![MockTurn::ToolCalls(calls)],
            Some(SessionStore::for_test(root.join("session.json"))),
        );
        session.cx.root = root;
        let pending_read = install_dispatch_probes(&mut session);
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let turn = session.user_turn(
            "read then write",
            cancel_rx,
            Arc::new(StdMutex::new(VecDeque::new())),
        );
        let interrupt = async {
            pending_read.notified().await;
            cancel_tx.send(true).unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let (result, ()) = tokio::join!(turn, interrupt);
            result.unwrap();
        })
        .await
        .expect("read batch should be interruptible");
        let batches = shared.pushed_tool_results.lock().unwrap();
        assert_eq!(batches.len(), 1);
        let results = &batches[0];
        assert_eq!(results.len(), 3);
        assert_eq!(results[0].id, "completed");
        assert_eq!(results[0].content, "0");
        assert!(!results[0].is_error);
        for id in ["pending", "unstarted"] {
            let result = results.iter().find(|r| r.id == id).unwrap();
            assert_eq!(result.content, INTERRUPTED_TOOL_RESULT);
            assert!(result.is_error);
        }
    }

    #[tokio::test]
    async fn early_cancel_and_zero_step_budget_preserve_accepted_user_input() {
        for cancelled in [false, true] {
            let (mut session, shared) = mk_session(vec![]);
            if !cancelled {
                session.max_turns = 0;
            }
            let (_cancel_tx, cancel_rx) = watch::channel(cancelled);
            session
                .user_turn(
                    "PRESERVE_ACCEPTED_INPUT",
                    cancel_rx,
                    Arc::new(StdMutex::new(VecDeque::new())),
                )
                .await
                .unwrap();
            assert_eq!(shared.started.load(Ordering::SeqCst), 0);
            assert!(
                shared
                    .pushed_users
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|text| text == "PRESERVE_ACCEPTED_INPUT")
            );
        }
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
    fn restored_reference_context_reestablishes_current_environment() {
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
        assert_eq!(pushed.len(), 2);
        assert!(pushed[0].contains("<environment_context>"));
        assert_eq!(pushed[1], "resume turn");
        assert_ne!(session.reference_context_item, Some(persisted));
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
    fn legacy_resumed_session_reestablishes_current_context() {
        let (session, _shared) = mk_session(vec![]);

        let restored = reference_context_item_for_restore(true, None, &session.cx);

        assert!(restored.is_none());
    }

    #[tokio::test]
    async fn automatic_and_overflow_compaction_restore_instructions_before_continuing() {
        use crate::context::dispatch::CompositionStrategy;
        for strategy in [
            CompositionStrategy::CodexShaped,
            CompositionStrategy::VibeShaped,
        ] {
            for overflow in [false, true] {
                let first = if overflow {
                    MockTurn::ContextOverflow
                } else {
                    MockTurn::HighUsageFollowUp
                };
                let (mut session, shared) = mk_session(vec![first, MockTurn::Text("done".into())]);
                session.strategy = strategy;
                let _directory =
                    install_startup_instructions(&mut session, "EXACT_COMPACTION_INSTRUCTIONS")
                        .await;
                session.compact_threshold = Some(5_000);
                run_user_turn(&mut session, "continue through compaction").await;
                assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 1);
                assert_eq!(shared.started.load(Ordering::SeqCst), 2);
                let requests = shared.seen_users.lock().unwrap();
                let systems = shared.seen_systems.lock().unwrap();
                if strategy.context_rides_user_lane() {
                    assert_eq!(
                        requests[1]
                            .iter()
                            .filter(|text| text.contains("EXACT_COMPACTION_INSTRUCTIONS"))
                            .count(),
                        2
                    );
                    ordered(
                        &requests[0].join("\n"),
                        &[
                            "EXACT_COMPACTION_INSTRUCTIONS",
                            "<environment_context>",
                            "continue through compaction",
                        ],
                    );
                    ordered(
                        &requests[1][requests[0].len()..].join("\n"),
                        &["EXACT_COMPACTION_INSTRUCTIONS", "<environment_context>"],
                    );
                } else {
                    assert_eq!(requests[0], vec!["continue through compaction"]);
                    assert_eq!(requests[1], requests[0]);
                    for system in systems.iter() {
                        assert!(
                            system
                                .stable_text()
                                .unwrap()
                                .contains("EXACT_COMPACTION_INSTRUCTIONS")
                        );
                    }
                }
                assert!(
                    session
                        .scoped_project_docs
                        .pending_batch(session.cx.instruction_generation)
                        .is_none()
                );
            }
        }
    }

    #[tokio::test]
    async fn compaction_clear_persists_null_and_next_turn_reinjects_full_context() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("ok".into())]);
        let _directory = install_startup_instructions(&mut session, "AGENTS_AFTER_COMPACT").await;
        session.reference_context_item = Some(
            crate::context::EnvironmentContext::from_tool_cx(&session.cx).to_turn_context_item(),
        );
        session.compact_manual().await.unwrap();
        assert_eq!(session.reference_context_item, None);
        assert_eq!(session.side_state()["reference_context"], Value::Null);
        run_user_turn(&mut session, "after compact").await;
        let requests = shared.seen_users.lock().unwrap();
        ordered(
            &requests[0].join("\n"),
            &[
                "AGENTS_AFTER_COMPACT",
                "<environment_context>",
                "after compact",
            ],
        );
        assert_eq!(
            requests[0].last().map(String::as_str),
            Some("after compact")
        );
        assert!(session.reference_context_item.is_some());
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

    #[tokio::test]
    async fn discovered_agents_move_to_context_before_environment() {
        let (mut session, shared) = mk_session(vec![]);
        let _directory = install_startup_instructions(&mut session, "AGENTS_UNIQUE_RULE").await;
        run_user_turn(&mut session, "hello").await;
        let requests = shared.seen_users.lock().unwrap();
        ordered(
            &requests[0].join("\n"),
            &["AGENTS_UNIQUE_RULE", "<environment_context>", "hello"],
        );
        assert_eq!(
            occurrences(&requests[0].join("\n"), "AGENTS_UNIQUE_RULE"),
            1
        );
        assert!(
            !shared.seen_systems.lock().unwrap()[0]
                .stable_text()
                .unwrap()
                .contains("AGENTS_UNIQUE_RULE")
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
        session.instruction_system = None;

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

    #[tokio::test]
    async fn codex_shaped_initial_context_orders_agents_scope_pins_env() {
        let (mut session, shared) = mk_session(vec![]);
        let _directory = install_startup_instructions(&mut session, "AGENTS_UNIQUE_RULE").await;
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        run_user_turn(&mut session, "hello").await;
        let systems = shared.seen_systems.lock().unwrap();
        let stable = systems[0].stable_text().unwrap();
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
        let requests = shared.seen_users.lock().unwrap();
        ordered(
            &requests[0].join("\n"),
            &[
                "AGENTS_UNIQUE_RULE",
                "<bbox_scope>",
                "task: task-1",
                "<bbox_pins>",
                "PINS_UNIQUE",
                "<environment_context>",
                "hello",
            ],
        );
        assert_eq!(requests[0].last().map(String::as_str), Some("hello"));
        assert!(session.dispatch.emitted_scope.is_some());
        assert!(session.dispatch.emitted_pins.is_some());
    }

    #[tokio::test]
    async fn vibe_shaped_folds_context_into_stable_and_keeps_user_lane_clean() {
        let (mut session, shared) = mk_session(vec![]);
        session.strategy = crate::context::dispatch::CompositionStrategy::VibeShaped;
        let _directory = install_startup_instructions(&mut session, "AGENTS_UNIQUE_RULE").await;
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        run_user_turn(&mut session, "one-line task").await;
        let systems = shared.seen_systems.lock().unwrap();
        let stable = systems[0].stable_text().unwrap();
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
        assert_eq!(shared.seen_users.lock().unwrap()[0], vec!["one-line task"]);
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

    #[tokio::test]
    async fn restored_dispatch_clear_revokes_history_before_new_task() {
        use crate::context::dispatch::{
            DispatchContextArg, DispatchState, resolve_dispatch_context_arg,
        };
        let (mut original, _) = mk_session(vec![]);
        original.dispatch = test_dispatch_state(Some(test_scope("old-task")));
        run_user_turn(&mut original, "original task").await;
        let persisted = serde_json::to_string(
            &json!({"side": original.side_state(), "history": original.tx.snapshot()}),
        )
        .unwrap();
        for clear in ["{}", ""] {
            let prior: Value = serde_json::from_str(&persisted).unwrap();
            let (mut resumed, shared) = mk_session(vec![]);
            resumed.tx.restore(prior["history"].clone());
            resumed.dispatch = DispatchState::from_arg(
                resolve_dispatch_context_arg(Some(clear)).unwrap(),
                &prior["side"],
            );
            run_user_turn(&mut resumed, "new task").await;
            let first = shared.seen_users.lock().unwrap()[0].join("\n");
            ordered(
                &first,
                &[
                    "task: old-task",
                    "Prior dispatch scope has been cleared",
                    "Prior dispatch pins have been cleared",
                    "new task",
                ],
            );
            assert_eq!(resumed.dispatch.emitted_to_side(), Value::Null);
            run_user_turn(&mut resumed, "next task").await;
            let requests = shared.seen_users.lock().unwrap();
            assert_eq!(
                occurrences(
                    &requests[1].join("\n"),
                    "Prior dispatch scope has been cleared"
                ),
                1
            );
            assert!(
                !shared.seen_systems.lock().unwrap()[0]
                    .stable_text()
                    .unwrap()
                    .contains("PERSONA_UNIQUE")
            );
        }
        for arg in [
            DispatchContextArg::Absent,
            DispatchContextArg::Provided(Box::new(original.dispatch.context.clone().unwrap())),
        ] {
            let (mut resumed, shared) = mk_session(vec![]);
            resumed.tx.restore(original.tx.snapshot());
            let absent = matches!(&arg, DispatchContextArg::Absent);
            resumed.dispatch = DispatchState::from_arg(arg, &original.side_state());
            run_user_turn(&mut resumed, "continue").await;
            let request = shared.seen_users.lock().unwrap()[0].join("\n");
            assert_eq!(
                request.contains("Prior dispatch scope has been cleared"),
                absent
            );
            assert!(!request.contains("Prior dispatch pins have been cleared"));
        }
        let (mut fresh, shared) = mk_session(vec![]);
        fresh.dispatch = DispatchState::from_arg(DispatchContextArg::Clear, &Value::Null);
        run_user_turn(&mut fresh, "fresh task").await;
        assert!(
            !shared.seen_users.lock().unwrap()[0]
                .join("\n")
                .contains("has been cleared")
        );
    }

    #[tokio::test]
    async fn instruction_updates_resume_and_revocations_follow_strategy_and_generation() {
        use crate::context::dispatch::CompositionStrategy;
        use bro_tools::{InstructionAccess, InstructionPaths, InstructionPolicy};
        for strategy in [
            CompositionStrategy::CodexShaped,
            CompositionStrategy::VibeShaped,
        ] {
            let (mut session, shared) = mk_session(vec![]);
            session.strategy = strategy;
            let _directory = install_startup_instructions(&mut session, "FIRST_RULE").await;
            run_user_turn(&mut session, "first task").await;
            let root = session.cx.root.clone();
            let old_generation = session.cx.instruction_generation;
            std::fs::write(root.join("AGENTS.md"), "UPDATED_RULE").unwrap();
            let mutation = || InstructionPaths {
                paths: vec![root.join("value.txt")],
                access: InstructionAccess::Mutate,
            };
            assert!(
                session
                    .scoped_project_docs
                    .check(mutation(), old_generation)
                    .await
                    .is_err()
            );
            run_user_turn(&mut session, "second task").await;
            assert!(
                session
                    .scoped_project_docs
                    .check(mutation(), old_generation)
                    .await
                    .is_err()
            );
            session
                .scoped_project_docs
                .check(mutation(), session.cx.instruction_generation)
                .await
                .unwrap();
            {
                let requests = shared.seen_users.lock().unwrap();
                let systems = shared.seen_systems.lock().unwrap();
                if strategy.context_rides_user_lane() {
                    ordered(
                        &requests[1].join("\n"),
                        &["FIRST_RULE", "first task", "UPDATED_RULE", "second task"],
                    );
                } else {
                    assert_eq!(requests[1], vec!["first task", "second task"]);
                    assert!(!systems[1].stable_text().unwrap().contains("FIRST_RULE"));
                    assert!(systems[1].stable_text().unwrap().contains("UPDATED_RULE"));
                }
            }

            run_user_turn(&mut session, "unchanged task").await;
            {
                let requests = shared.seen_users.lock().unwrap();
                assert_eq!(requests[2].len(), requests[1].len() + 1);
                assert_eq!(
                    requests[2].last().map(String::as_str),
                    Some("unchanged task")
                );
                if !strategy.context_rides_user_lane() {
                    let systems = shared.seen_systems.lock().unwrap();
                    assert_eq!(systems[2].stable_text(), systems[1].stable_text());
                }
            }

            // Reconstruct from serialized side state and history; no delivery
            // receipts or captured system text survive this process boundary.
            let side: Value =
                serde_json::from_str(&serde_json::to_string(&session.side_state()).unwrap())
                    .unwrap();
            let (mut resumed, resumed_shared) = mk_session(vec![]);
            resumed.strategy = strategy;
            resumed.cx.root = root.clone();
            resumed.tx.restore(session.tx.snapshot());
            resumed.scoped_project_docs = Arc::new(startup_docs_at(&root).await);
            resumed.scoped_project_docs.restore_observed_paths(
                serde_json::from_value(side["instruction_observed_paths"].clone()).unwrap(),
            );
            resumed
                .scoped_project_docs
                .restore_documents(
                    serde_json::from_value(side["instruction_documents"].clone()).unwrap(),
                )
                .await
                .unwrap();
            resumed.cx.instruction_policy = Some(resumed.scoped_project_docs.clone());
            resumed.resume_runtime_reset = true;
            std::fs::write(root.join("AGENTS.md"), "RESUMED_RULE").unwrap();
            run_user_turn(&mut resumed, "resumed task").await;
            let generation = resumed.cx.instruction_generation;
            resumed
                .scoped_project_docs
                .check(mutation(), generation)
                .await
                .unwrap();
            std::fs::remove_file(root.join("AGENTS.md")).unwrap();
            assert!(
                resumed
                    .scoped_project_docs
                    .check(mutation(), generation)
                    .await
                    .is_err()
            );
            run_user_turn(&mut resumed, "after removal").await;
            resumed
                .scoped_project_docs
                .check(mutation(), resumed.cx.instruction_generation)
                .await
                .unwrap();
            let requests = resumed_shared.seen_users.lock().unwrap();
            let systems = resumed_shared.seen_systems.lock().unwrap();
            if strategy.context_rides_user_lane() {
                ordered(
                    &requests[0].join("\n"),
                    &["Session runtime reset", "RESUMED_RULE", "resumed task"],
                );
                ordered(
                    &requests[1].join("\n"),
                    &[
                        "resumed task",
                        "revoke its previously delivered instructions",
                        "after removal",
                    ],
                );
            } else {
                assert!(systems[0].stable_text().unwrap().contains("RESUMED_RULE"));
                assert!(!systems[1].stable_text().unwrap().contains("RESUMED_RULE"));
                assert!(
                    systems[1]
                        .stable_text()
                        .unwrap()
                        .contains("revoke its previously delivered instructions")
                );
                assert!(!requests[1].join("\n").contains("RESUMED_RULE"));
            }
        }
    }

    #[test]
    fn removed_dispatch_scope_is_explicitly_cleared_once() {
        let (mut session, shared) = mk_session(vec![]);
        session.dispatch = test_dispatch_state(None);
        session.dispatch.emitted_scope = Some("<bbox_scope>task: old</bbox_scope>".into());
        session.push_user_text("follow-up");
        assert!(session.dispatch.emitted_scope.is_none());
        let users = shared.pushed_users.lock().unwrap();
        assert_eq!(
            users
                .iter()
                .filter(|text| text.contains("Prior dispatch scope has been cleared"))
                .count(),
            1
        );
        drop(users);
        let count = shared.pushed_users.lock().unwrap().len();
        session.prepare_context_for_user_turn();
        assert_eq!(shared.pushed_users.lock().unwrap().len(), count);
    }

    #[tokio::test]
    async fn codex_shaped_post_compaction_re_emits_current_context() {
        let (mut session, shared) = mk_session(vec![]);
        let _directory = install_startup_instructions(&mut session, "AGENTS_UNIQUE_RULE").await;
        session.dispatch = test_dispatch_state(Some(test_scope("task-1")));
        run_user_turn(&mut session, "turn one").await;
        session.dispatch.context.as_mut().unwrap().scope = Some(test_scope("task-9"));
        session.compact_manual().await.unwrap();
        run_user_turn(&mut session, "turn two").await;
        let requests = shared.seen_users.lock().unwrap();
        ordered(
            &requests[1][requests[0].len()..].join("\n"),
            &[
                "AGENTS_UNIQUE_RULE",
                "<bbox_scope>",
                "task: task-9",
                "<bbox_pins>",
                "<environment_context>",
                "turn two",
            ],
        );
        assert_eq!(
            session.dispatch.emitted_scope.as_deref(),
            session.dispatch.scope_render().as_deref()
        );
    }

    #[test]
    fn suppressed_defaults_with_dispatch_context_keeps_persona_and_directives() {
        // `--system-prompt ""` clears explicit_system AND disables AGENTS
        // discovery, but a dispatch context still lands persona + directives
        // in stable (design §8): base + persona + directives, no AGENTS.
        let (mut session, _shared) = mk_session(vec![]);
        session.explicit_system = None;
        session.instruction_system = None;
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
        drop(tx);
        // Pending turns are distinct accepted tasks. Live stdin arriving while
        // a task is active intentionally becomes a mid-turn steer instead.
        let pending = VecDeque::from(["alpha".into(), "beta".into()]);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, pending)
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
    async fn exit_when_idle_waits_for_delayed_first_stdin_turn() {
        let (mut session, shared) = mk_session(vec![MockTurn::Text("done".into())]);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            tx.send(Input::User("delayed prompt".into())).unwrap();
        });
        let ctrl = Emitter::new("ctrl".into());
        let mut pending = VecDeque::new();

        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            await_first_controlled_input(&mut session, &mut rx, &ctrl, &mut pending),
        )
        .await
        .expect("worker must wait for the daemon's first stdin message")
        .unwrap();
        writer.await.unwrap();
        assert_eq!(pending.front().map(String::as_str), Some("delayed prompt"));

        session_loop_until_idle(&mut session, rx, &ctrl, pending)
            .await
            .unwrap();
        let users = shared.pushed_users.lock().unwrap().clone();
        assert_eq!(
            user_turns_after_initial_context(&users),
            &["delayed prompt".to_string()]
        );
        assert_eq!(shared.completed.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn max_turns_is_per_user_turn_in_persistent_session() {
        let (mut session, shared) =
            mk_session(vec![MockTurn::Text("1".into()), MockTurn::Text("2".into())]);
        session.max_turns = 1;
        let (tx, rx) = mpsc::unbounded_channel();
        drop(tx);
        // Pending turns are distinct accepted tasks. Live stdin arriving while
        // a task is active intentionally becomes a mid-turn steer instead.
        let pending = VecDeque::from(["alpha".into(), "beta".into()]);
        let ctrl = Emitter::new("ctrl".into());
        session_loop(&mut session, rx, &ctrl, pending)
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
        let started = shared.started.clone();
        let sender = tokio::spawn(async move {
            while started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
            tx.send(Input::Control {
                subtype: "interrupt".into(),
                req_id: Some("r1".into()),
                raw: json!({}),
            })
            .unwrap();
        });
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
        sender.await.unwrap();
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
    fn est_tokens_counts_utf8_bytes_and_rounds_up() {
        assert_eq!(est_tokens(""), 0);
        assert_eq!(est_tokens("abcd"), 1);
        assert_eq!(est_tokens("abc"), 1);
        assert_eq!(est_tokens("你好世界"), 3);
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
    fn control_recorder(session: &mut Session) -> (Emitter, Arc<Mutex<Vec<Value>>>, Arc<Notify>) {
        let path = session.store.store_path().clone();
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        let rejected = Arc::new(Notify::new());
        let rejected_callback = rejected.clone();
        let log = Arc::new(EventLog::at_path(path.with_extension("events.jsonl")));
        session.event_log = log.clone();
        session.emitter = make_emitter(
            "controls".into(),
            Some(Arc::new(|_| {})),
            Some(log.clone()),
            session.seq_counter(),
        );
        let emitter = make_emitter(
            "controls".into(),
            Some(Arc::new(move |event| {
                let snapshot = std::fs::read_to_string(&path)
                    .ok()
                    .and_then(|text| serde_json::from_str::<Value>(&text).ok());
                let is_error = event["response"]["subtype"] == "error";
                captured
                    .lock()
                    .unwrap()
                    .push(json!({"event":event,"snapshot_at_ack":snapshot}));
                if is_error {
                    rejected_callback.notify_one();
                }
            })),
            Some(log),
            session.seq_counter(),
        );
        (emitter, events, rejected)
    }

    #[tokio::test]
    async fn idle_controls_validate_then_persist_before_acknowledging() {
        let (mut session, _) = mk_session(vec![]);
        let (ctrl, events, _) = control_recorder(&mut session);
        let mut pending = VecDeque::from(["older input".to_string()]);
        for (subtype, raw, id) in [
            (
                "set_max_thinking_tokens",
                json!({"value":12}),
                "unsupported",
            ),
            ("set_model", json!({"model":false}), "wrong-type"),
            ("set_model", json!({"model":" "}), "empty"),
            ("interrupt", json!({"prompt":42}), "bad-redirect"),
        ] {
            assert!(receive_control(subtype, &raw, Some(id.into()), &ctrl).is_none());
        }
        assert_eq!(session.base_opts.model, "m");
        let set_model = receive_control(
            "set_model",
            &json!({"request":{"model":"new-model"}}),
            Some("model".into()),
            &ctrl,
        )
        .unwrap();
        apply_pending_control(&mut session, set_model, &ctrl, &mut pending)
            .await
            .unwrap();
        let interrupt = receive_control(
            "interrupt",
            &json!({"prompt":"redirect first"}),
            Some("redirect".into()),
            &ctrl,
        )
        .unwrap();
        apply_pending_control(&mut session, interrupt, &ctrl, &mut pending)
            .await
            .unwrap();
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 6);
        assert!(
            events[..4]
                .iter()
                .all(|event| event["event"]["response"]["subtype"] == "error")
        );
        assert_eq!(events[4]["snapshot_at_ack"]["model"], "new-model");
        assert_eq!(
            events[5]["snapshot_at_ack"]["side"]["pending_user_inputs"],
            json!(["redirect first", "older input"])
        );
        let restored = restore_pending_user_inputs(&events[5]["snapshot_at_ack"]["side"]).unwrap();
        assert_eq!(restored, pending);
        assert!(
            events[5]["snapshot_at_ack"]["event_log_offset"]
                .as_u64()
                .unwrap()
                > 0
        );
    }

    #[tokio::test]
    async fn active_controls_defer_ack_and_durably_prioritize_redirects() {
        let (mut session, shared) = mk_session(vec![MockTurn::Block]);
        let (ctrl, events, rejected) = control_recorder(&mut session);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            let mut pending = VecDeque::new();
            run_prompt_with_controls(&mut session, &mut rx, &ctrl, &mut pending, "initial".into())
                .await
                .unwrap();
            (session, pending)
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while shared.started.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tx.send(Input::Control {
            subtype: "set_model".into(),
            req_id: Some("model".into()),
            raw: json!({"model":"after-turn"}),
        })
        .unwrap();
        tx.send(Input::Control {
            subtype: "unknown".into(),
            req_id: Some("barrier".into()),
            raw: json!({}),
        })
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), rejected.notified())
            .await
            .unwrap();
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|row| row["event"]["response"]["subtype"] == "error")
        );
        tx.send(Input::User("older steer".into())).unwrap();
        tx.send(Input::User("/compact".into())).unwrap();
        tx.send(Input::Control {
            subtype: "interrupt".into(),
            req_id: Some("stop".into()),
            raw: json!({"prompt":"redirect"}),
        })
        .unwrap();
        let (session, pending) = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            pending,
            VecDeque::from([
                "redirect".to_owned(),
                "older steer".to_owned(),
                "/compact".to_owned()
            ])
        );
        assert_eq!(session.base_opts.model, "after-turn");
        assert_eq!(shared.compact_calls.load(Ordering::SeqCst), 0);
        assert!(
            !shared
                .pushed_users
                .lock()
                .unwrap()
                .iter()
                .any(|text| text == "/compact")
        );
        let events = events.lock().unwrap();
        let model = events
            .iter()
            .find(|row| row["event"]["response"]["request_id"] == "model")
            .unwrap();
        assert_eq!(model["snapshot_at_ack"]["model"], "after-turn");
        let stop = events
            .iter()
            .find(|row| row["event"]["response"]["request_id"] == "stop")
            .unwrap();
        assert_eq!(
            stop["snapshot_at_ack"]["side"]["pending_user_inputs"],
            json!(["redirect", "older steer", "/compact"])
        );
    }

    #[tokio::test]
    async fn interrupt_during_compaction_waits_for_mutation_and_keeps_queued_input() {
        let (mut session, shared) = mk_session(vec![]);
        shared.compact_block.store(true, Ordering::SeqCst);
        let (ctrl, events, rejected) = control_recorder(&mut session);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let run = tokio::spawn(async move {
            let mut pending = VecDeque::new();
            run_prompt_with_controls(
                &mut session,
                &mut rx,
                &ctrl,
                &mut pending,
                "/compact".into(),
            )
            .await
            .unwrap();
            (session, pending)
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while shared.compact_calls.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        tx.send(Input::User("queued after compact".into())).unwrap();
        tx.send(Input::Control {
            subtype: "interrupt".into(),
            req_id: Some("stop".into()),
            raw: json!({}),
        })
        .unwrap();
        tx.send(Input::Control {
            subtype: "unknown".into(),
            req_id: Some("barrier".into()),
            raw: json!({}),
        })
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), rejected.notified())
            .await
            .unwrap();
        assert!(
            !run.is_finished(),
            "interrupt cannot drop an active native compaction mutation"
        );
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .all(|row| row["event"]["response"]["subtype"] != "success")
        );
        shared.compact_gate.notify_one();
        let (_, pending) = tokio::time::timeout(std::time::Duration::from_secs(5), run)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending, VecDeque::from(["queued after compact".to_owned()]));
        assert_eq!(shared.started.load(Ordering::SeqCst), 0);
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|row| row["event"]["response"]["request_id"] == "stop"
                    && row["snapshot_at_ack"]["side"]["pending_user_inputs"]
                        == json!(["queued after compact"]))
        );
    }

    #[tokio::test]
    async fn failed_persistence_emits_control_error_without_success_ack() {
        let (mut session, _) = mk_session(vec![]);
        let (ctrl, events, _) = control_recorder(&mut session);
        std::fs::create_dir(session.store.store_path()).unwrap();
        let control = receive_control(
            "set_model",
            &json!({"model":"changed"}),
            Some("persist-failure".into()),
            &ctrl,
        )
        .unwrap();
        assert!(
            apply_pending_control(&mut session, control, &ctrl, &mut VecDeque::new())
                .await
                .is_err()
        );
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"]["response"]["subtype"], "error");
        assert!(
            events[0]["event"]["response"]["error"]
                .as_str()
                .unwrap()
                .contains("persistence failed")
        );
    }

    #[tokio::test]
    async fn manual_compaction_failure_is_visible_and_snapshot_is_preserved() {
        let (mut session, shared) = mk_session(vec![]);
        shared.compact_fail.store(true, Ordering::SeqCst);
        let events = Arc::new(Mutex::new(Vec::new()));
        let captured = events.clone();
        session.emitter = Emitter::with_callback(
            "compact-failure".into(),
            Arc::new(move |event| {
                captured.lock().unwrap().push(event);
            }),
        );
        let (_tx, mut rx) = mpsc::unbounded_channel();
        let ctrl = Emitter::with_callback("control".into(), Arc::new(|_| {}));
        run_prompt_with_controls(
            &mut session,
            &mut rx,
            &ctrl,
            &mut VecDeque::new(),
            "/compact".into(),
        )
        .await
        .unwrap();
        assert!(events.lock().unwrap().iter().any(|event| {
            event["type"] == "result"
                && event["is_error"] == true
                && event["result"]
                    .as_str()
                    .unwrap()
                    .contains("synthetic compaction failure")
        }));
        assert!(session.store.store_path().exists());
        assert_eq!(shared.started.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn queued_resume_inputs_reject_malformed_side_state() {
        assert!(restore_pending_user_inputs(&json!({"pending_user_inputs":"lost queue"})).is_err());
        assert!(restore_pending_user_inputs(&json!({"pending_user_inputs":[false]})).is_err());
        assert!(restore_pending_user_inputs(&json!({})).unwrap().is_empty());
    }

    #[tokio::test]
    async fn failed_event_log_never_gets_a_successful_control_ack() {
        let (mut session, _) = mk_session(vec![]);
        let (ctrl, events, _) = control_recorder(&mut session);
        std::fs::create_dir(session.event_log.path()).unwrap();
        let control = receive_control(
            "set_model",
            &json!({"model":"changed"}),
            Some("log-failure".into()),
            &ctrl,
        )
        .unwrap();
        assert!(
            apply_pending_control(&mut session, control, &ctrl, &mut VecDeque::new())
                .await
                .is_err()
        );
        assert!(!session.store.store_path().exists());
        let events = events.lock().unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["event"]["response"]["subtype"], "error");
    }
    #[tokio::test]
    async fn unresolved_remote_source_stops_provider_loop_even_when_wrapper_hides_error() {
        const MARKER: &str = "MCP server fixture: deadline_exceeded; remote completion unknown";
        struct RemoteSource(Arc<std::sync::atomic::AtomicBool>);
        #[async_trait]
        impl Tool for RemoteSource {
            fn name(&self) -> &str {
                "mcp__fixture__mutate"
            }
            fn description(&self) -> &str {
                "Synthetic remote outcome source"
            }
            fn input_schema(&self) -> Value {
                json!({"type":"object"})
            }
            async fn call(&self, _: Value, _: &ToolCx) -> bro_tools::ToolResult {
                self.0.store(true, Ordering::SeqCst);
                bro_tools::ToolResult::Error(json!({"content":[{"type":"text","text":"Do not retry; remote completion unknown"}],"structuredContent":{"code":"mcp_remote_outcome_unknown"},"isError":true}).to_string())
            }
            fn uncertain_outcome(&self) -> Option<String> {
                self.0.load(Ordering::SeqCst).then(|| MARKER.to_owned())
            }
        }
        struct Wrapper {
            source: Arc<dyn Tool>,
            catches_error: bool,
        }
        #[async_trait]
        impl Tool for Wrapper {
            fn name(&self) -> &str {
                self.source.name()
            }
            fn description(&self) -> &str {
                "Wrapper without an uncertainty accessor"
            }
            fn input_schema(&self) -> Value {
                self.source.input_schema()
            }
            async fn call(&self, input: Value, cx: &ToolCx) -> bro_tools::ToolResult {
                let result = self.source.call(input, cx).await;
                if self.catches_error {
                    bro_tools::ToolResult::Json(json!({"error_was_caught":true}))
                } else {
                    result
                }
            }
        }
        for catches_error in [false, true] {
            let source: Arc<dyn Tool> = Arc::new(RemoteSource(Arc::new(
                std::sync::atomic::AtomicBool::new(false),
            )));
            let wrapper: Arc<dyn Tool> = Arc::new(Wrapper {
                source: source.clone(),
                catches_error,
            });
            let (mut session, shared) = mk_session(vec![
                MockTurn::ToolCalls(vec![dispatch_call("remote-call", source.name(), json!({}))]),
                MockTurn::Text("must not call the provider again".into()),
            ]);
            // The original connection sources survive wrapper replacement and
            // multiple aliases, so no wrapper forwarding is needed for safety.
            session.remote_outcome_sources = vec![source.clone(), source];
            session.reg = Registry::new(
                vec![wrapper.clone()],
                vec![],
                &PinPolicy::default(),
                &mcp::ToolFilter::default(),
            )
            .unwrap();
            let events = Arc::new(Mutex::new(Vec::new()));
            let captured = events.clone();
            session.emitter = Emitter::with_callback(
                "unknown-remote".into(),
                Arc::new(move |event| {
                    captured.lock().unwrap().push(event);
                }),
            );
            run_user_turn(&mut session, "call the remote fixture").await;
            assert_eq!(
                shared.started.load(Ordering::SeqCst),
                1,
                "no provider continuation after uncertainty"
            );
            assert!(
                wrapper.uncertain_outcome().is_none(),
                "fixture intentionally does not forward metadata"
            );
            assert_eq!(session.uncertain_remote_outcomes(), vec![MARKER.to_owned()]);
            let terminal = events
                .lock()
                .unwrap()
                .iter()
                .find(|event| event["type"] == "result")
                .unwrap()
                .clone();
            assert_eq!(terminal["subtype"], "incomplete");
            assert_eq!(terminal["is_error"], true);
            assert_eq!(terminal["stop_reason"], "remote_outcome_unknown");
            assert!(
                !events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event["type"] == "result" && event["subtype"] == "success")
            );
            assert_eq!(
                shared.pushed_tool_results.lock().unwrap().len(),
                1,
                "actual tool receipt still reaches history"
            );
            session.persist().await.unwrap();
            let persisted: Value =
                serde_json::from_str(&std::fs::read_to_string(session.store.store_path()).unwrap())
                    .unwrap();
            assert_eq!(
                persisted["side"]["remote_outcomes_unknown"],
                json!([MARKER])
            );
        }
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

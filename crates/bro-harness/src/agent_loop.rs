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
//!   (design/orchestration/fleet-tui.md §2). Wire shapes follow the Claude Agent
//!   SDK control protocol (hyperclaude SDK_PROTOCOL.md / NDJSON_FORMAT.md).
//!
//! The transport handles all wire differences; the loop and the stdout envelope
//! are identical across providers.

use crate::cli::Cli;
use crate::emit::Emitter;
use crate::hooks::{Delivery, HookEngine, NudgeLedger};
use crate::mcp;
use crate::registry::{PinPolicy, Registry};
use crate::session::{SaveState, SessionStore};
use crate::transport::{self, StopReason, SystemPrompt, Transport, TransportKind, TurnOpts, Usage};
use anyhow::{Context, Result};
use bro_tools::{SafetyPolicy, ToolCx, builtin_tools};
use serde_json::{Value, json};
use std::collections::{HashSet, VecDeque};
use std::io::Read as _;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt as _;
use tokio::sync::{mpsc, watch};

const DEFAULT_MAX_TOKENS: u32 = 8192;
/// Hard backstop on loop iterations *per user turn*; the daemon's supervision is
/// the outer guard. Override with `BRO_HARNESS_MAX_TURNS`.
const DEFAULT_MAX_TURNS: u64 = 50;

/// Marker injected as a tool_result when a tool dispatch is interrupted, so the
/// transport buffer stays valid (every tool_use gets a matching result).
const INTERRUPTED_TOOL_RESULT: &str = "[Request interrupted by user]";

/// Entry point. Branches one-shot vs. bidirectional on `--input-format`.
pub async fn run(cli: Cli) -> Result<()> {
    if cli.input_format.as_deref() == Some("stream-json") {
        return run_session(cli).await;
    }

    // One-shot: a single prompt, one user turn, then persist and exit.
    let prompt = resolve_prompt(&cli)?;
    let mut session = Session::build(&cli).await?;
    session.emitter.system_init();
    // A cancel channel that never fires — one-shot turns are not interruptible.
    let (_cancel_tx, cancel_rx) = watch::channel(false);
    session.user_turn(&prompt, cancel_rx).await?;
    session.persist()?;
    Ok(())
}

/// Bidirectional persistent session driven over stdin NDJSON.
async fn run_session(cli: Cli) -> Result<()> {
    let replay = cli.replay_user_messages;
    let mut session = Session::build(&cli).await?;
    session.emitter.system_init_session();
    let sid = session.session_id().to_string();

    // The stdin reader runs as its own task so control messages (interrupt)
    // arrive while a turn is in flight. It owns a clone of the emitter purely to
    // honour `--replay-user-messages`.
    let input_rx = spawn_stdin_reader(replay, Emitter::new(sid.clone()));
    // A separate emitter for control responses emitted *during* a turn, when the
    // session's own emitter is borrowed by the running turn.
    let ctrl_emitter = Emitter::new(sid);

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
                Some(Input::Control { subtype, req_id, raw }) => {
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
        // (cancels the turn) or a steer (queues for the next boundary). The
        // stdin arms must NOT touch `session` — it's borrowed by the turn.
        let (cancel_tx, cancel_rx) = watch::channel(false);
        let mut stdin_closed = false;
        let mut deferred: Vec<(String, Value)> = Vec::new();
        {
            let turn = session.user_turn(&prompt, cancel_rx);
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
                        Some(Input::Control { subtype, req_id, .. }) if subtype == "interrupt" => {
                            let _ = cancel_tx.send(true);
                            ctrl_emitter.control_response_success(req_id.as_deref());
                        }
                        Some(Input::User(p)) => pending.push_back(p),
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
        if stdin_closed && pending.is_empty() {
            break;
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
    clipboard: Arc<std::sync::Mutex<bro_tools::Registers>>,
    // Mutable accumulators carried across user turns.
    total_usage: Usage,
    turns: u64,
    last_prompt_tokens: u64,
    /// Volatile system-tail nudge to surface on the upcoming model call.
    tail_nudge: Option<String>,
}

impl Session {
    async fn build(cli: &Cli) -> Result<Self> {
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

        // Empty --system-prompt means "suppress" (provider-defaults mode).
        let system = match cli.system_prompt.as_deref() {
            Some("") | None => None,
            Some(s) => Some(s.to_string()),
        };

        let kind = TransportKind::from_env();
        let mut tx = transport::build_transport(kind).await?;

        let store = SessionStore::open(cli.session_id.as_deref(), cli.resume.as_deref())?;
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
        let clipboard = Arc::new(std::sync::Mutex::new(bro_tools::Registers::from_side(
            prior_side.get("clipboard").unwrap_or(&Value::Null),
        )));
        let hooks = HookEngine::from_env(NudgeLedger::from_side(
            prior_side.get("nudges").unwrap_or(&Value::Null),
        ));
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
            .or_else(|| std::env::var("ANTHROPIC_MODEL").ok())
            .or_else(|| std::env::var("BRO_HARNESS_MODEL").ok())
            .context(
                "no --model, no resumed session model, and no ANTHROPIC_MODEL/BRO_HARNESS_MODEL",
            )?;

        let cx = ToolCx {
            root: std::env::current_dir().context("cwd")?,
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(std::sync::Mutex::new(bro_tools::ShellSessions::default())),
            clipboard: clipboard.clone(),
        };
        // The builtin `report` tool is harness-owned (it emits the cockpit's
        // status signal on the stream) and holds its own emitter handle. It is
        // registered always but only pinned in fleet (bidirectional) mode.
        let fleet = cli.input_format.as_deref() == Some("stream-json");
        let mut builtins = builtin_tools();
        builtins.push(Arc::new(crate::report::ReportTool::new(Emitter::new(
            store.id.clone(),
        ))));
        let tool_filter =
            mcp::ToolFilter::from_csv(cli.deny_tools.as_deref(), cli.allow_tools.as_deref());
        let mcp_tools = mcp::load_mcp_tools(cli.mcp_config.as_deref(), &tool_filter).await;
        let mut pin = PinPolicy::from_env();
        if fleet {
            pin.also_pin(crate::report::REPORT_TOOL);
        }
        let reg = Registry::new(builtins, mcp_tools, &pin, &tool_filter);

        let base_opts = TurnOpts {
            model,
            max_tokens,
            system: SystemPrompt::default(),
            effort: cli.effort.clone(),
            web_search,
        };

        let emitter = Emitter::new(store.id.clone());
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
            clipboard,
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
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
        match self
            .tx
            .compact(
                self.compaction.keep_tail(),
                crate::compaction::COMPACTION_INSTRUCTION,
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
    async fn user_turn(&mut self, prompt: &str, mut cancel: watch::Receiver<bool>) -> Result<()> {
        self.tx.push_user_text(prompt);
        for n in self.hooks.on_user_turn(prompt) {
            if n.delivery == Delivery::SystemTail {
                self.tail_nudge = Some(n.message);
            }
        }

        let mut final_text = String::new();

        loop {
            if self.turns >= self.max_turns {
                tracing::warn!(max_turns = self.max_turns, "hit max turns; stopping");
                break;
            }
            if *cancel.borrow() {
                break;
            }

            // Compact before composing when the previous prompt crossed the
            // model's window threshold.
            if let Some(thresh) = self.compact_threshold
                && self.last_prompt_tokens > thresh
            {
                match self
                    .tx
                    .compact(
                        self.compaction.keep_tail(),
                        crate::compaction::COMPACTION_INSTRUCTION,
                        &self.base_opts,
                    )
                    .await
                {
                    Ok(Some(summary)) => {
                        tracing::info!(pre_tokens = self.last_prompt_tokens, "compacted");
                        self.emitter.compact_boundary(
                            "auto",
                            self.last_prompt_tokens,
                            summary.len(),
                        );
                    }
                    Ok(None) => {}
                    Err(e) => tracing::warn!("compaction failed: {e:#}"),
                }
            }

            let tool_specs = self.reg.wire_specs();
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

            let out = tokio::select! {
                biased;
                _ = cancel.changed() => break,
                r = self.tx.run_turn(&tool_specs, &opts, &self.emitter) => r?,
            };
            self.turns += 1;
            self.total_usage.add(&out.usage);
            self.last_prompt_tokens = out.usage.total_input_tokens();

            for n in self.hooks.on_assistant_turn(&out.text, &out.tool_calls) {
                if n.delivery == Delivery::SystemTail {
                    self.tail_nudge = Some(n.message);
                }
            }

            // Full assistant turn (text + tool_use) for the daemon tail / fleet
            // transcript; the daemon dedupes text against streamed deltas.
            let mut assistant_content: Vec<Value> = Vec::new();
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
                break;
            }

            // Dispatch tool calls, interruptibly. On interrupt mid-dispatch, pad
            // every not-yet-resolved call with an interrupted marker so the
            // assistant(tool_use) message keeps a matching tool_result.
            let mut results: Vec<transport::ToolResult> = Vec::with_capacity(out.tool_calls.len());
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
                        for n in self.hooks.on_tool_result(tc, &result) {
                            match n.delivery {
                                Delivery::Rider => result.content.push_str(&n.rider_block()),
                                Delivery::SystemTail => self.tail_nudge = Some(n.message),
                            }
                        }
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
                    }
                }
                self.emitter.tool_results(&results);
                self.tx.push_tool_results(results);
                break;
            }

            self.emitter.tool_results(&results);
            self.tx.push_tool_results(results);
            self.hooks.tick();
        }

        self.emitter
            .result(&final_text, &self.total_usage, self.turns, None);
        Ok(())
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
        side["clipboard"] = self
            .clipboard
            .lock()
            .map(|c| c.to_side())
            .unwrap_or(Value::Null);
        self.store.save(&SaveState {
            transport: self.tx.name(),
            model: &self.base_opts.model,
            snapshot: self.tx.snapshot(),
            side,
        })
    }
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

/// Compose the effective system prompt as a cache-stable prefix plus a volatile
/// tail. See the transport `SystemPrompt` docs.
fn compose_system(base: Option<&str>, reg: &Registry) -> SystemPrompt {
    let mut stable = base.unwrap_or("").to_string();

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
        /// Await a gate that tests never release — to be cancelled by interrupt.
        Block,
    }

    #[derive(Clone, Default)]
    struct MockShared {
        pushed_users: Arc<Mutex<Vec<String>>>,
        started: Arc<AtomicUsize>,
        completed: Arc<AtomicUsize>,
        compact_calls: Arc<AtomicUsize>,
    }

    struct MockTransport {
        shared: MockShared,
        scripts: Arc<Mutex<VecDeque<MockTurn>>>,
        gate: Arc<Notify>,
    }

    #[async_trait]
    impl Transport for MockTransport {
        fn name(&self) -> &'static str {
            "mock"
        }
        fn push_user_text(&mut self, text: &str) {
            self.shared.pushed_users.lock().unwrap().push(text.to_string());
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
                    self.gate.notified().await;
                    unreachable!("gate is never released in tests");
                }
                MockTurn::Text(t) => {
                    self.shared.completed.fetch_add(1, Ordering::SeqCst);
                    Ok(transport::TurnOutput {
                        text: t,
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
            _keep_tail: usize,
            _instruction: &str,
            _opts: &TurnOpts,
        ) -> Result<Option<String>> {
            self.shared.compact_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Some("summary".into()))
        }
    }

    fn mk_session(scripts: Vec<MockTurn>) -> (Session, MockShared) {
        let shared = MockShared::default();
        let mock = MockTransport {
            shared: shared.clone(),
            scripts: Arc::new(Mutex::new(scripts.into_iter().collect())),
            gate: Arc::new(Notify::new()),
        };
        let todos = Arc::new(Mutex::new(bro_tools::TodoList::default()));
        let clipboard = Arc::new(Mutex::new(bro_tools::Registers::default()));
        let cx = ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: todos.clone(),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            clipboard: clipboard.clone(),
        };
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let id = format!("bh-test-{}-{}", std::process::id(), nanos);
        let session = Session {
            tx: Box::new(mock),
            reg: Registry::new(vec![], vec![], &PinPolicy::from_env(), &mcp::ToolFilter::default()),
            cx,
            hooks: HookEngine::from_env(NudgeLedger::from_side(&Value::Null)),
            emitter: Emitter::new("test".into()),
            base_opts: TurnOpts {
                model: "m".into(),
                max_tokens: 8,
                system: SystemPrompt::default(),
                effort: None,
                web_search: false,
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
            clipboard,
            total_usage: Usage::default(),
            turns: 0,
            last_prompt_tokens: 0,
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
}

//! The native session driver: typed commands in, Agent Client Protocol
//! across, normalized worker events out.
//!
//! One task owns all state and handles one input at a time. The server holds
//! a `session/prompt` request open for the whole turn, so prompt answers
//! arrive on their own channel; the loop drains session updates before it
//! reads an answer, which keeps a turn's last updates ahead of its result.
//!
//! A prompt sent while another is running joins the running turn: the server
//! answers the earlier request at once and carries the turn on under the new
//! one. A turn therefore ends when every prompt sent for it has been
//! answered.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::rpc::{Notification, Rpc, RpcError};

const PROTOCOL_VERSION: u64 = 1;

/// Everything a session needs from the dispatch, resolved before the first
/// turn.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// The session to continue; `None` starts a new one.
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub system_prompt_append: Option<String>,
    pub cwd: String,
    /// The dispatch's MCP servers in protocol form.
    pub mcp_servers: Vec<Value>,
    /// Their names (reported in `init`).
    pub mcp_server_names: Vec<String>,
}

/// Initialize the connection. Nothing else may be sent before this
/// succeeds.
pub async fn handshake(rpc: &Rpc, resuming: bool) -> Result<Value, RpcError> {
    let result = rpc
        .request(
            "initialize",
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "clientCapabilities": {},
                "clientInfo": {
                    "name": "bro-worker",
                    "title": "bro-worker",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
        .await?;
    if result["protocolVersion"].as_u64() != Some(PROTOCOL_VERSION) {
        return Err(RpcError {
            message: format!(
                "agy speaks ACP protocol version {}, not {PROTOCOL_VERSION}",
                result["protocolVersion"]
            ),
        });
    }
    if resuming
        && result["agentCapabilities"]["sessionCapabilities"]
            .get("resume")
            .is_none()
    {
        return Err(RpcError {
            message: "agy does not offer session/resume".into(),
        });
    }
    Ok(result)
}

pub fn warn(message: &str) {
    eprintln!("agy adapter: {message}");
}

/// One user input waiting for a turn.
#[derive(Debug)]
enum Queued {
    Text { text: String, envelope: Value },
    Compact { envelope: Value },
}

#[derive(Debug, Default)]
struct TokenSums {
    input: u64,
    cached: u64,
    cache_write: u64,
    output: u64,
}

impl TokenSums {
    /// A prompt answer's usage. Its `inputTokens` excludes cached input.
    fn add(&mut self, usage: &Value) {
        let field = |key: &str| usage[key].as_u64().unwrap_or(0);
        self.input += field("inputTokens");
        self.cached += field("cachedReadTokens");
        self.cache_write += field("cachedWriteTokens");
        self.output += field("outputTokens");
    }

    fn to_envelope(&self) -> Value {
        json!({
            "input_tokens": self.input,
            "cache_read_input_tokens": self.cached,
            "cache_creation_input_tokens": self.cache_write,
            "output_tokens": self.output,
        })
    }
}

#[derive(Debug)]
struct Turn {
    started: Instant,
    usage: TokenSums,
    model_requests: u64,
    /// Prompt requests sent for this turn and not yet answered.
    in_flight: usize,
    last_text: Option<String>,
    error: Option<String>,
    /// A stop reason other than a normal end or a cancellation.
    stopped: Option<String>,
    cancelled: bool,
    interrupt_requested: bool,
    compact: bool,
}

impl Turn {
    fn new(compact: bool) -> Self {
        Self {
            started: Instant::now(),
            usage: TokenSums::default(),
            model_requests: 0,
            in_flight: 0,
            last_text: None,
            error: None,
            stopped: None,
            cancelled: false,
            interrupt_requested: false,
            compact,
        }
    }
}

struct Driver {
    rpc: Rpc,
    cfg: SessionConfig,
    out: mpsc::UnboundedSender<Value>,
    answers: mpsc::UnboundedSender<Result<Value, RpcError>>,
    session_id: Option<String>,
    session_error: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    turn: Option<Turn>,
    queue: VecDeque<Queued>,
    /// The assistant message being streamed, when one is open.
    message: Option<String>,
    messages: u64,
    thinking: String,
    announced_tools: HashSet<String>,
    finished_tools: HashSet<String>,
}

/// Drive one dispatch until end of input and the last turn's result, then
/// close the server's stdin. Returns early when the server exits.
pub async fn run_session(
    cfg: SessionConfig,
    rpc: Rpc,
    mut notifications: mpsc::UnboundedReceiver<Notification>,
    mut input: mpsc::UnboundedReceiver<Value>,
    out: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let (answers, mut answered) = mpsc::unbounded_channel();
    let mut driver = Driver {
        rpc,
        model: cfg.model.clone(),
        effort: cfg.effort.clone(),
        cfg,
        out,
        answers,
        session_id: None,
        session_error: None,
        turn: None,
        queue: VecDeque::new(),
        message: None,
        messages: 0,
        thinking: String::new(),
        announced_tools: HashSet::new(),
        finished_tools: HashSet::new(),
    };
    let mut input_closed = false;
    loop {
        if input_closed && driver.turn.is_none() && driver.queue.is_empty() {
            break;
        }
        tokio::select! {
            biased;
            note = notifications.recv() => match note {
                Some(note) => driver.handle_notification(note).await?,
                None => {
                    driver.server_gone().await?;
                    anyhow::bail!("agy ACP server closed its output");
                }
            },
            Some(answer) = answered.recv() => driver.prompt_answered(answer).await?,
            envelope = input.recv(), if !input_closed => match envelope {
                Some(envelope) => driver.handle_input(serde_json::from_value(envelope)?).await?,
                None => input_closed = true,
            },
        }
    }
    driver.rpc.close().await;
    Ok(())
}

impl Driver {
    // ── stdout ──────────────────────────────────────────────────────────

    async fn emit(&mut self, mut event: Value) -> anyhow::Result<()> {
        if let Some(object) = event.as_object_mut()
            && !object.contains_key("session_id")
            && let Some(session) = self.session_id.as_ref().or(self.cfg.resume.as_ref())
        {
            object.insert("session_id".into(), Value::String(session.clone()));
        }
        self.out
            .send(event)
            .map_err(|_| anyhow::anyhow!("agy event writer closed"))
    }

    async fn replay(&mut self, mut envelope: Value) -> anyhow::Result<()> {
        if let Some(object) = envelope.as_object_mut() {
            object.entry("parent_tool_use_id").or_insert(Value::Null);
        }
        self.emit(envelope).await
    }

    async fn stream(&mut self, event: Value) -> anyhow::Result<()> {
        self.emit(json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}))
            .await
    }

    async fn assistant(&mut self, id: &str, content: Value) -> anyhow::Result<()> {
        let message = json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": content,
            "stop_reason": null,
        });
        self.emit(json!({"type": "assistant", "message": message, "parent_tool_use_id": null}))
            .await
    }

    /// A `result` for input that never became a turn.
    async fn error_result(&mut self, message: &str) -> anyhow::Result<()> {
        self.emit(json!({
            "type": "result",
            "subtype": "error",
            "is_error": true,
            "result": message,
            "num_turns": 0,
            "duration_ms": 0,
        }))
        .await
    }

    async fn control_response(
        &mut self,
        request_id: &Value,
        error: Option<String>,
    ) -> anyhow::Result<()> {
        let response = match error {
            None => json!({"subtype": "success", "request_id": request_id, "response": {}}),
            Some(error) => json!({"subtype": "error", "request_id": request_id, "error": error}),
        };
        self.emit(json!({"type": "control_response", "response": response}))
            .await
    }

    // ── stdin ───────────────────────────────────────────────────────────

    async fn handle_input(&mut self, input: bro_protocol::SessionInput) -> anyhow::Result<()> {
        let request_id = json!(input.request_id);
        match input.command {
            bro_protocol::SessionCommand::UserTurn { text } => {
                anyhow::ensure!(!text.trim().is_empty(), "empty user turn");
                let envelope = user_envelope(&text);
                self.submit(Queued::Text { text, envelope }).await
            }
            bro_protocol::SessionCommand::Compact => {
                self.submit(Queued::Compact {
                    envelope: user_envelope("/compact"),
                })
                .await
            }
            bro_protocol::SessionCommand::Interrupt => {
                let error = self.cancel().await.err().map(|error| error.to_string());
                self.control_response(&request_id, error).await
            }
            bro_protocol::SessionCommand::SetModel { model } => {
                let error = self.set_model(model).await.err();
                self.control_response(&request_id, error).await
            }
        }
    }

    /// Cancel the running turn. Its prompt requests answer `cancelled`.
    async fn cancel(&mut self) -> Result<(), RpcError> {
        let (Some(session), Some(turn)) = (self.session_id.clone(), self.turn.as_mut()) else {
            return Ok(());
        };
        if turn.interrupt_requested {
            return Ok(());
        }
        turn.interrupt_requested = true;
        self.rpc
            .notify("session/cancel", json!({"sessionId": session}))
            .await
    }

    async fn set_model(&mut self, model: String) -> Result<(), String> {
        if model.trim().is_empty() {
            return Err("model must not be empty".into());
        }
        if let Some(session) = self.session_id.clone() {
            self.set_option(&session, "model", &model)
                .await
                .map_err(|error| error.to_string())?;
        }
        self.model = Some(model);
        Ok(())
    }

    async fn set_option(
        &self,
        session: &str,
        option: &str,
        value: &str,
    ) -> Result<Value, RpcError> {
        self.rpc
            .request(
                "session/set_config_option",
                json!({"sessionId": session, "configId": option, "value": value}),
            )
            .await
    }

    async fn submit(&mut self, item: Queued) -> anyhow::Result<()> {
        if self.session_id.is_none() {
            if let Some(error) = self.session_error.clone() {
                return self.error_result(&error).await;
            }
            if let Err(error) = self.open_session().await {
                let message = format!("cannot open the agy session: {error}");
                warn(&message);
                self.session_error = Some(message.clone());
                return self.error_result(&message).await;
            }
        }
        // A user turn during a running turn joins it, unless the turn is
        // being cancelled or is a compaction, or earlier input is already
        // waiting for the next turn.
        if let Queued::Text { text, envelope } = &item
            && self.queue.is_empty()
            && self
                .turn
                .as_ref()
                .is_some_and(|turn| !turn.compact && !turn.interrupt_requested)
        {
            match self.send_prompt(std::slice::from_ref(text)).await {
                Ok(()) => return self.replay(envelope.clone()).await,
                Err(error) => warn(&format!(
                    "steering prompt failed, queueing for the next turn: {error}"
                )),
            }
        }
        self.queue.push_back(item);
        if self.turn.is_none() {
            self.start_next().await?;
        }
        Ok(())
    }

    /// Send one prompt for the running turn and route its answer back to
    /// the loop.
    async fn send_prompt(&mut self, texts: &[String]) -> Result<(), RpcError> {
        let blocks: Vec<Value> = texts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect();
        let reply = self
            .rpc
            .call(
                "session/prompt",
                json!({"sessionId": self.session_id, "prompt": blocks}),
            )
            .await?;
        if let Some(turn) = self.turn.as_mut() {
            turn.in_flight += 1;
        }
        let answers = self.answers.clone();
        tokio::spawn(async move {
            let _ = answers.send(reply.wait().await);
        });
        Ok(())
    }

    async fn open_session(&mut self) -> Result<(), RpcError> {
        let mut params = json!({"cwd": self.cfg.cwd, "mcpServers": self.cfg.mcp_servers});
        if let Some(text) = &self.cfg.system_prompt_append {
            params["_meta"] = json!({"antigravity": {"systemPrompt": {"append": text}}});
        }
        let method = match &self.cfg.resume {
            Some(session) => {
                params["sessionId"] = json!(session);
                "session/resume"
            }
            None => "session/new",
        };
        let result = self.rpc.request(method, params).await?;
        let session = result["sessionId"]
            .as_str()
            .map(str::to_string)
            .or_else(|| self.cfg.resume.clone())
            .ok_or_else(|| RpcError {
                message: format!("{method} returned no session id"),
            })?;
        // Each model offers its own thinking levels, so the model is chosen
        // first and the level is set against the options that choice leaves.
        let mut options = result["configOptions"].clone();
        if let Some(model) = self.model.clone() {
            options = self.set_option(&session, "model", &model).await?["configOptions"].clone();
        }
        if let Some(effort) = self.effort.clone() {
            if current_option(&options, "thought_level").is_some() {
                options =
                    self.set_option(&session, "thought_level", &effort).await?["configOptions"]
                        .clone();
            } else {
                warn(&format!(
                    "the session's model has no thinking levels; effort {effort} is not applied"
                ));
                self.effort = None;
            }
        }
        if let Some(model) = current_option(&options, "model") {
            self.model.get_or_insert_with(|| model.to_string());
        }
        if let Some(effort) = current_option(&options, "thought_level") {
            self.effort.get_or_insert_with(|| effort.to_string());
        }
        self.session_id = Some(session.clone());
        let init = json!({
            "type": "system",
            "subtype": "init",
            "session_id": session,
            "cwd": self.cfg.cwd,
            "model": self.model,
            "reasoning_effort": self.effort,
            "provider": bro_core::Provider::Agy.as_str(),
            "permissionMode": "bypassPermissions",
            "tools": [],
            "mcp_servers": self.cfg.mcp_server_names.iter().map(|name| json!({"name": name, "status": "configured"})).collect::<Vec<_>>(),
            "resumed": self.cfg.resume.is_some(),
        });
        self.emit(init).await.map_err(|error| RpcError {
            message: error.to_string(),
        })
    }

    /// Start the next turn from the queue: a compaction alone, or every
    /// consecutive user text as one prompt.
    async fn start_next(&mut self) -> anyhow::Result<()> {
        while self.turn.is_none() {
            let Some(first) = self.queue.pop_front() else {
                return Ok(());
            };
            let (texts, envelopes, compact) = match first {
                Queued::Compact { envelope } => {
                    (vec!["/compact".to_string()], vec![envelope], true)
                }
                Queued::Text { text, envelope } => {
                    let mut texts = vec![text];
                    let mut envelopes = vec![envelope];
                    while let Some(Queued::Text { .. }) = self.queue.front() {
                        if let Some(Queued::Text { text, envelope }) = self.queue.pop_front() {
                            texts.push(text);
                            envelopes.push(envelope);
                        }
                    }
                    (texts, envelopes, false)
                }
            };
            for envelope in envelopes {
                self.replay(envelope).await?;
            }
            self.turn = Some(Turn::new(compact));
            if let Err(error) = self.send_prompt(&texts).await {
                self.turn = None;
                self.error_result(&format!("session/prompt failed: {error}"))
                    .await?;
            }
        }
        Ok(())
    }

    // ── prompt answers ──────────────────────────────────────────────────

    async fn prompt_answered(&mut self, answer: Result<Value, RpcError>) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        turn.in_flight = turn.in_flight.saturating_sub(1);
        match answer {
            Ok(result) => {
                turn.usage.add(&result["usage"]);
                match result["stopReason"].as_str() {
                    Some("end_turn") => {}
                    Some("cancelled") => turn.cancelled = true,
                    Some(reason) => turn.stopped = Some(reason.to_string()),
                    None => turn.error = Some("session/prompt returned no stop reason".into()),
                }
            }
            Err(error) => turn.error = Some(error.message),
        }
        if turn.in_flight > 0 {
            return Ok(());
        }
        self.finish_turn().await
    }

    async fn finish_turn(&mut self) -> anyhow::Result<()> {
        self.flush().await?;
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        let text = turn
            .last_text
            .as_deref()
            .unwrap_or_default()
            .trim()
            .to_string();
        let mut result = json!({
            "type": "result",
            "duration_ms": turn.started.elapsed().as_millis() as u64,
            "num_turns": turn.model_requests.max(1),
            "usage": turn.usage.to_envelope(),
        });
        if let Some(message) = turn.error {
            result["subtype"] = json!("error");
            result["is_error"] = json!(true);
            result["result"] = json!(message);
        } else if turn.cancelled {
            result["subtype"] = json!("interrupted");
            result["interrupted"] = json!(true);
            result["is_error"] = json!(false);
            result["result"] = json!(text);
        } else if let Some(reason) = turn.stopped {
            result["subtype"] = json!("error");
            result["is_error"] = json!(true);
            result["result"] = json!(format!("agy stopped the turn: {reason}"));
        } else {
            result["subtype"] = json!("success");
            result["is_error"] = json!(false);
            result["result"] = json!(text);
        }
        if turn.compact && result["result"] == "" {
            result["result"] = json!("Compacted.");
        }
        self.emit(result).await?;
        self.start_next().await
    }

    // ── session updates ─────────────────────────────────────────────────

    async fn handle_notification(&mut self, note: Notification) -> anyhow::Result<()> {
        if note.method != "session/update" {
            return Ok(());
        }
        let params = &note.params;
        if self.session_id.as_deref() != params["sessionId"].as_str() {
            return Ok(());
        }
        let update = &params["update"];
        match update["sessionUpdate"].as_str() {
            Some("agent_message_chunk") => {
                let Some(text) = update["content"]["text"].as_str() else {
                    return Ok(());
                };
                self.flush_thinking().await?;
                self.open_message().await?;
                self.stream(json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": {"type": "text_delta", "text": text},
                }))
                .await?;
                if let Some(message) = self.message.as_mut() {
                    message.push_str(text);
                }
            }
            Some("agent_thought_chunk") => {
                if let Some(text) = update["content"]["text"].as_str() {
                    self.flush_message().await?;
                    self.thinking.push_str(text);
                }
            }
            Some("tool_call") | Some("tool_call_update") => {
                self.flush().await?;
                self.tool_update(update).await?;
            }
            Some("usage_update") => {
                let used = update["used"].as_u64().unwrap_or(0);
                if used > 0
                    && let Some(turn) = self.turn.as_mut()
                {
                    turn.model_requests += 1;
                }
                let mut context = serde_json::Map::new();
                if used > 0 {
                    context.insert("last_turn_input_tokens".into(), json!(used));
                }
                if let Some(size) = update["size"].as_u64().filter(|size| *size > 0) {
                    context.insert("context_window".into(), json!(size));
                }
                if !context.is_empty() {
                    self.emit(json!({
                        "type": "system",
                        "subtype": "context_pressure",
                        "context": context,
                    }))
                    .await?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn open_message(&mut self) -> anyhow::Result<()> {
        if self.message.is_some() {
            return Ok(());
        }
        self.messages += 1;
        self.message = Some(String::new());
        let id = self.message_id();
        let model = self.model.clone();
        self.stream(json!({
            "type": "message_start",
            "message": {"id": id, "type": "message", "role": "assistant", "model": model, "content": []},
        }))
        .await?;
        self.stream(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }))
        .await
    }

    fn message_id(&self) -> String {
        format!("agy-message-{}", self.messages)
    }

    /// Commit whatever is being accumulated: the protocol marks no end of a
    /// message or a thought, so the next kind of update closes it.
    async fn flush(&mut self) -> anyhow::Result<()> {
        self.flush_thinking().await?;
        self.flush_message().await
    }

    async fn flush_message(&mut self) -> anyhow::Result<()> {
        let Some(text) = self.message.take() else {
            return Ok(());
        };
        self.stream(json!({"type": "content_block_stop", "index": 0}))
            .await?;
        self.stream(json!({"type": "message_stop"})).await?;
        if let Some(turn) = self.turn.as_mut()
            && !text.trim().is_empty()
        {
            turn.last_text = Some(text.clone());
        }
        let id = self.message_id();
        self.assistant(&id, json!([{"type": "text", "text": text}]))
            .await
    }

    async fn flush_thinking(&mut self) -> anyhow::Result<()> {
        let text = std::mem::take(&mut self.thinking);
        let text = text.trim();
        if text.is_empty() {
            return Ok(());
        }
        self.messages += 1;
        let id = self.message_id();
        self.assistant(
            &id,
            json!([{"type": "thinking", "thinking": text, "signature": ""}]),
        )
        .await
    }

    /// Announce a tool call once, and report its result once it completes
    /// or fails.
    async fn tool_update(&mut self, update: &Value) -> anyhow::Result<()> {
        let Some(id) = update["toolCallId"].as_str().map(str::to_string) else {
            return Ok(());
        };
        if self.announced_tools.insert(id.clone()) {
            let name = update["name"]
                .as_str()
                .or_else(|| update["title"].as_str())
                .unwrap_or("tool");
            let input = [&update["rawInput"], &update["args"]]
                .into_iter()
                .find(|input| input.is_object())
                .cloned()
                .unwrap_or_else(|| json!({}));
            self.assistant(
                &id,
                json!([{"type": "tool_use", "id": id, "name": name, "input": input}]),
            )
            .await?;
        }
        let failed = match update["status"].as_str() {
            Some("completed") => false,
            Some("failed") => true,
            _ => return Ok(()),
        };
        if !self.finished_tools.insert(id.clone()) {
            return Ok(());
        }
        self.emit(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": tool_output(update, failed),
                    "is_error": failed,
                }],
            },
            "parent_tool_use_id": null,
        }))
        .await
    }

    /// The server exited mid-session: end the running turn with an error
    /// and stop.
    async fn server_gone(&mut self) -> anyhow::Result<()> {
        let message = "the agy ACP server exited";
        warn(message);
        if self.turn.take().is_some() || !self.queue.is_empty() {
            self.queue.clear();
            self.flush().await?;
            self.error_result(message).await?;
        }
        Ok(())
    }
}

fn user_envelope(text: &str) -> Value {
    json!({"type": "user", "message": {"role": "user", "content": [{"type": "text", "text": text}]}})
}

/// The current value of one session option.
fn current_option<'a>(options: &'a Value, id: &str) -> Option<&'a str> {
    options
        .as_array()?
        .iter()
        .find(|option| option["id"] == id)?["currentValue"]
        .as_str()
}

/// A finished tool call's output as text. The server reports none for some
/// tools, MCP calls among them.
fn tool_output(update: &Value, failed: bool) -> String {
    let raw = &update["rawOutput"];
    let text = if let Some(text) = raw.as_str().or_else(|| raw["output"].as_str()) {
        text.to_string()
    } else if raw.is_object() {
        raw.to_string()
    } else {
        update["content"]
            .as_array()
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block["content"]["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default()
    };
    if text.is_empty() && failed {
        return "the tool call failed".to_string();
    }
    text
}

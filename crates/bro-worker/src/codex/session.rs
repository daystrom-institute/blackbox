//! The native session driver: typed commands in, app-server RPC across,
//! normalized worker events out.
//!
//! One task owns all state and handles one input at a time, so a turn's
//! notifications are translated in the order the app-server sent them and
//! every stdout line is written whole. Requests are awaited inline; the
//! notifications that arrive meanwhile wait in their channel.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::rpc::{Notification, Rpc, RpcError};

/// Everything a session needs from the dispatch, resolved before the first
/// turn.
#[derive(Debug, Clone, Default)]
pub struct SessionConfig {
    /// The thread to continue; `None` starts a new one.
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub config_overrides: std::collections::BTreeMap<String, Value>,
    pub developer_instructions: Option<String>,
    pub base_instructions: Option<String>,
    pub output_schema: Option<Value>,
    /// The thread's sandbox mode: `danger-full-access` when the dispatch
    /// skips permissions, `workspace-write` otherwise.
    pub sandbox: &'static str,
    pub include_partial_messages: bool,
    pub replay_user_messages: bool,
    pub cwd: String,
    /// The MCP servers the dispatch defined (reported in `init`).
    pub mcp_servers: Vec<String>,
    /// Servers of the app-server's own config that this dispatch disables
    /// (`--strict-mcp-config`).
    pub disabled_mcp_servers: Vec<String>,
}

/// Initialize the connection. Nothing else may be sent before this
/// succeeds.
pub async fn handshake(rpc: &Rpc) -> Result<Value, RpcError> {
    let result = rpc
        .request(
            "initialize",
            json!({
                "clientInfo": {
                    "name": "bro-worker",
                    "title": "bro-worker",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            }),
        )
        .await?;
    rpc.notify("initialized", json!({})).await?;
    Ok(result)
}

/// The app-server's own MCP servers that a strict dispatch turns off: every
/// server its effective config defines that the dispatch does not.
pub async fn servers_to_disable(
    rpc: &Rpc,
    cwd: &str,
    ours: &[String],
) -> Result<Vec<String>, RpcError> {
    let result = rpc.request("config/read", json!({"cwd": cwd})).await?;
    let config = result["config"].as_object().ok_or_else(|| RpcError {
        message: "config/read returned no configuration object".into(),
    })?;
    let Some(servers) = config.get("mcp_servers") else {
        return Ok(Vec::new());
    };
    let servers = servers.as_object().ok_or_else(|| RpcError {
        message: "config/read returned malformed MCP servers".into(),
    })?;
    if servers.keys().any(|name| !super::mcp::valid_name(name)) {
        return Err(RpcError {
            message: "config/read returned an unsupported MCP server name".into(),
        });
    }
    Ok(servers
        .keys()
        .filter(|name| !ours.contains(name))
        .cloned()
        .collect())
}

pub fn warn(message: &str) {
    eprintln!("codex adapter: {message}");
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
    fn add(&mut self, breakdown: &Value) {
        let field = |key: &str| breakdown[key].as_u64().unwrap_or(0);
        self.input += field("inputTokens");
        self.cached += field("cachedInputTokens");
        self.cache_write += field("cacheWriteInputTokens");
        self.output += field("outputTokens");
    }

    /// The Anthropic-native shape the daemon parses: `input_tokens` is fresh
    /// input, cache reads and writes ride their own fields. The app-server's
    /// `inputTokens` includes cached input.
    fn to_envelope(&self) -> Value {
        json!({
            "input_tokens": self.input.saturating_sub(self.cached).saturating_sub(self.cache_write),
            "cache_read_input_tokens": self.cached,
            "cache_creation_input_tokens": self.cache_write,
            "output_tokens": self.output,
        })
    }
}

#[derive(Debug)]
struct Turn {
    id: Option<String>,
    started: Instant,
    usage: TokenSums,
    model_requests: u64,
    last_text: Option<String>,
    final_text: Option<String>,
    error: Option<Value>,
    interrupt_requested: bool,
    interrupt_sent: bool,
    compact: bool,
    open_messages: HashSet<String>,
    announced_tools: HashSet<String>,
}

impl Turn {
    fn new(id: Option<String>, compact: bool) -> Self {
        Self {
            id,
            started: Instant::now(),
            usage: TokenSums::default(),
            model_requests: 0,
            last_text: None,
            final_text: None,
            error: None,
            interrupt_requested: false,
            interrupt_sent: false,
            compact,
            open_messages: HashSet::new(),
            announced_tools: HashSet::new(),
        }
    }
}

struct Driver {
    rpc: Rpc,
    cfg: SessionConfig,
    out: mpsc::UnboundedSender<Value>,
    thread_id: Option<String>,
    thread_error: Option<String>,
    model: Option<String>,
    turn: Option<Turn>,
    queue: VecDeque<Queued>,
    last_request_usage: Option<TokenSums>,
}

/// Drive one dispatch until end of input and the last turn's result, then
/// close the app-server's stdin. Returns early when the app-server exits.
pub async fn run_session(
    cfg: SessionConfig,
    rpc: Rpc,
    mut notifications: mpsc::UnboundedReceiver<Notification>,
    mut input: mpsc::UnboundedReceiver<Value>,
    out: mpsc::UnboundedSender<Value>,
) -> anyhow::Result<()> {
    let mut driver = Driver {
        rpc,
        model: cfg.model.clone(),
        cfg,
        out,
        thread_id: None,
        thread_error: None,
        turn: None,
        queue: VecDeque::new(),
        last_request_usage: None,
    };
    let mut input_closed = false;
    loop {
        if input_closed && driver.turn.is_none() && driver.queue.is_empty() {
            break;
        }
        tokio::select! {
            envelope = input.recv(), if !input_closed => match envelope {
                Some(envelope) => driver.handle_input(serde_json::from_value(envelope)?).await?,
                None => input_closed = true,
            },
            note = notifications.recv() => match note {
                Some(note) => driver.handle_notification(note).await?,
                None => {
                    driver.server_gone().await?;
                    anyhow::bail!("Codex app-server closed its output");
                }
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
            && let Some(session) = self.thread_id.as_ref().or(self.cfg.resume.as_ref())
        {
            object.insert("session_id".into(), Value::String(session.clone()));
        }
        self.out
            .send(event)
            .map_err(|_| anyhow::anyhow!("Codex event writer closed"))
    }

    async fn replay(&mut self, envelope: Value) -> anyhow::Result<()> {
        if !self.cfg.replay_user_messages {
            return Ok(());
        }
        let mut envelope = envelope;
        if let (Some(object), Some(thread)) = (envelope.as_object_mut(), self.thread_id.as_ref()) {
            object.insert("session_id".into(), Value::String(thread.clone()));
            object.entry("parent_tool_use_id").or_insert(Value::Null);
        }
        self.emit(envelope).await
    }

    async fn stream(&mut self, event: Value) -> anyhow::Result<()> {
        if !self.cfg.include_partial_messages {
            return Ok(());
        }
        self.emit(json!({"type": "stream_event", "event": event, "parent_tool_use_id": null}))
            .await
    }

    async fn assistant(
        &mut self,
        id: &str,
        content: Value,
        usage: Option<Value>,
    ) -> anyhow::Result<()> {
        let mut message = json!({
            "id": id,
            "type": "message",
            "role": "assistant",
            "model": self.model,
            "content": content,
            "stop_reason": null,
        });
        if let Some(usage) = usage {
            message["usage"] = usage;
        }
        self.emit(json!({"type": "assistant", "message": message, "parent_tool_use_id": null}))
            .await
    }

    async fn tool_result(
        &mut self,
        id: &str,
        content: Value,
        is_error: bool,
    ) -> anyhow::Result<()> {
        self.emit(json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": id,
                    "content": content,
                    "is_error": is_error,
                }],
            },
            "parent_tool_use_id": null,
        }))
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

    async fn handle_input(&mut self, input: bro_protocol::CodexInput) -> anyhow::Result<()> {
        let request_id = json!(input.request_id);
        let command = input.command;
        match command {
            bro_protocol::SessionCommand::UserTurn { text } => {
                anyhow::ensure!(!text.trim().is_empty(), "empty user turn");
                let envelope = json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":text}]}});
                self.submit(Queued::Text { text, envelope }).await
            }
            bro_protocol::SessionCommand::Compact => {
                self.submit(Queued::Compact {
                    envelope: json!({"type":"user","message":{"role":"user","content":[{"type":"text","text":"/compact"}]}}),
                })
                .await
            }
            bro_protocol::SessionCommand::Interrupt => {
                if let Some(turn) = self.turn.as_mut() {
                    turn.interrupt_requested = true;
                }
                let error = self
                    .send_interrupt()
                    .await
                    .err()
                    .map(|error| error.to_string());
                self.control_response(&request_id, error).await
            }
            bro_protocol::SessionCommand::SetModel { model } => {
                let error = if model.trim().is_empty() {
                    Some("model must not be empty".into())
                } else {
                    self.model = Some(model);
                    None
                };
                self.control_response(&request_id, error).await
            }
        }
    }

    async fn send_interrupt(&mut self) -> Result<(), RpcError> {
        let Some(thread) = self.thread_id.clone() else {
            return Ok(());
        };
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        let Some(turn_id) = turn
            .id
            .clone()
            .filter(|_| turn.interrupt_requested && !turn.interrupt_sent)
        else {
            return Ok(());
        };
        self.rpc
            .request(
                "turn/interrupt",
                json!({"threadId": thread, "turnId": turn_id}),
            )
            .await?;
        if let Some(turn) = self.turn.as_mut() {
            turn.interrupt_sent = true;
        }
        Ok(())
    }

    async fn submit(&mut self, item: Queued) -> anyhow::Result<()> {
        if self.thread_id.is_none() {
            if let Some(error) = self.thread_error.clone() {
                return self.error_result(&error).await;
            }
            if let Err(error) = self.open_thread().await {
                let message = format!("cannot open the codex thread: {error}");
                warn(&message);
                self.thread_error = Some(message.clone());
                return self.error_result(&message).await;
            }
        }
        // A user turn during a running turn steers it, unless the turn is
        // being interrupted or is a compaction, or earlier input is already
        // waiting for the next turn.
        if let Queued::Text { text, envelope } = &item
            && self.queue.is_empty()
            && let Some(turn) = self.turn.as_ref()
            && !turn.compact
            && !turn.interrupt_requested
            && let Some(turn_id) = turn.id.clone()
        {
            let steered = self
                .rpc
                .request(
                    "turn/steer",
                    json!({
                        "threadId": self.thread_id,
                        "expectedTurnId": turn_id,
                        "input": [text_input(text)],
                    }),
                )
                .await;
            match steered {
                Ok(_) => return self.replay(envelope.clone()).await,
                // The turn most likely ended before the steer landed; the
                // input starts the next one.
                Err(error) => warn(&format!(
                    "turn/steer failed, queueing for the next turn: {error}"
                )),
            }
        }
        self.queue.push_back(item);
        if self.turn.is_none() {
            self.start_next().await?;
        }
        Ok(())
    }

    async fn open_thread(&mut self) -> Result<(), RpcError> {
        let mut params = json!({
            "cwd": self.cfg.cwd,
            "approvalPolicy": "never",
            "sandbox": self.cfg.sandbox,
        });
        if let Some(model) = &self.cfg.model {
            params["model"] = json!(model);
        }
        if let Some(text) = &self.cfg.developer_instructions {
            params["developerInstructions"] = json!(text);
        }
        if let Some(text) = &self.cfg.base_instructions {
            params["baseInstructions"] = json!(text);
        }
        if !self.cfg.disabled_mcp_servers.is_empty() || !self.cfg.config_overrides.is_empty() {
            let mut config: serde_json::Map<String, Value> = self
                .cfg
                .disabled_mcp_servers
                .iter()
                .map(|name| (format!("mcp_servers.{name}.enabled"), Value::Bool(false)))
                .collect();
            config.extend(self.cfg.config_overrides.clone());
            params["config"] = Value::Object(config);
        }
        if let Some(tier) = &self.cfg.service_tier {
            params["serviceTier"] = json!(tier);
        }
        let method = match &self.cfg.resume {
            Some(thread) => {
                params["threadId"] = json!(thread);
                "thread/resume"
            }
            None => "thread/start",
        };
        let result = self.rpc.request(method, params).await?;
        let thread = result["thread"]["id"]
            .as_str()
            .or_else(|| result["threadId"].as_str())
            .map(str::to_string)
            .ok_or_else(|| RpcError {
                message: format!("{method} returned no thread id"),
            })?;
        self.thread_id = Some(thread.clone());
        if let Some(model) = result["model"].as_str() {
            self.model.get_or_insert_with(|| model.to_string());
        }
        let init = json!({
            "type": "system",
            "subtype": "init",
            "session_id": thread,
            "cwd": result["cwd"].as_str().unwrap_or(&self.cfg.cwd),
            "model": self.model,
            "reasoning_effort": self.cfg.effort.clone().or_else(|| result["reasoningEffort"].as_str().map(str::to_string)),
            "provider": bro_core::Provider::Codex.as_str(),
            "permissionMode": if self.cfg.sandbox == "danger-full-access" { "bypassPermissions" } else { "default" },
            "tools": [],
            "mcp_servers": self.cfg.mcp_servers.iter().map(|name| json!({"name": name, "status": "configured"})).collect::<Vec<_>>(),
            "resumed": self.cfg.resume.is_some(),
        });
        self.emit(init).await.map_err(|error| RpcError {
            message: error.to_string(),
        })
    }

    /// Start the next turn from the queue: a compaction alone, or every
    /// consecutive user text as one turn's input.
    async fn start_next(&mut self) -> anyhow::Result<()> {
        while self.turn.is_none() {
            let Some(first) = self.queue.pop_front() else {
                return Ok(());
            };
            let thread = self.thread_id.clone();
            match first {
                Queued::Compact { envelope } => {
                    self.replay(envelope).await?;
                    match self
                        .rpc
                        .request("thread/compact/start", json!({"threadId": thread}))
                        .await
                    {
                        Ok(_) => self.turn = Some(Turn::new(None, true)),
                        Err(error) => {
                            self.error_result(&format!("compaction failed: {error}"))
                                .await?
                        }
                    }
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
                    for envelope in envelopes {
                        self.replay(envelope).await?;
                    }
                    let mut params = json!({
                        "threadId": thread,
                        "input": texts.iter().map(|text| text_input(text)).collect::<Vec<_>>(),
                    });
                    if let Some(model) = &self.model {
                        params["model"] = json!(model);
                    }
                    if let Some(effort) = &self.cfg.effort {
                        params["effort"] = json!(effort);
                    }
                    if let Some(tier) = &self.cfg.service_tier {
                        params["serviceTier"] = json!(tier);
                    }
                    if let Some(schema) = &self.cfg.output_schema {
                        params["outputSchema"] = schema.clone();
                    }
                    match self.rpc.request("turn/start", params).await {
                        Ok(result) => {
                            let id = result["turn"]["id"].as_str().map(str::to_string);
                            self.turn = Some(Turn::new(id, false));
                        }
                        Err(error) => {
                            self.error_result(&format!("turn/start failed: {error}"))
                                .await?
                        }
                    }
                }
            }
        }
        Ok(())
    }

    // ── app-server notifications ────────────────────────────────────────

    async fn handle_notification(&mut self, note: Notification) -> anyhow::Result<()> {
        let params = &note.params;
        if let (Some(ours), Some(theirs)) = (self.thread_id.as_deref(), params["threadId"].as_str())
            && ours != theirs
        {
            return Ok(());
        }
        // Turn-scoped notifications for a turn other than the running one
        // are stale.
        if let (Some(turn), Some(theirs)) = (
            self.turn.as_ref(),
            params["turnId"]
                .as_str()
                .or_else(|| params["turn"]["id"].as_str()),
        ) && let Some(ours) = turn.id.as_deref()
            && ours != theirs
        {
            return Ok(());
        }
        match note.method.as_str() {
            "turn/started" => {
                if let Some(turn) = self.turn.as_mut()
                    && turn.id.is_none()
                {
                    turn.id = params["turn"]["id"].as_str().map(str::to_string);
                }
                self.send_interrupt().await?;
            }
            "item/started" => self.item_started(&params["item"]).await?,
            "item/agentMessage/delta" => {
                let item = params["itemId"].as_str().unwrap_or_default().to_string();
                self.open_message(&item).await?;
                if let Some(delta) = params["delta"].as_str() {
                    self.stream(json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": delta},
                    }))
                    .await?;
                }
            }
            "item/completed" => self.item_completed(&params["item"]).await?,
            "thread/tokenUsage/updated" => {
                let last = &params["tokenUsage"]["last"];
                let mut request = TokenSums::default();
                request.add(last);
                if let Some(turn) = self.turn.as_mut() {
                    turn.usage.add(last);
                    turn.model_requests += 1;
                }
                self.last_request_usage = Some(request);
            }
            "error" => {
                let message = params["error"]["message"]
                    .as_str()
                    .unwrap_or("unknown error");
                if params["willRetry"].as_bool() == Some(true) {
                    warn(&format!("codex is retrying after: {message}"));
                } else if let Some(turn) = self.turn.as_mut() {
                    turn.error = Some(params["error"].clone());
                }
            }
            "turn/completed"
                if self.turn.as_ref().and_then(|turn| turn.id.as_deref())
                    == params["turn"]["id"].as_str() =>
            {
                self.finish_turn(&params["turn"]).await?;
            }
            _ => {}
        }
        Ok(())
    }

    async fn open_message(&mut self, item: &str) -> anyhow::Result<()> {
        let Some(turn) = self.turn.as_mut() else {
            return Ok(());
        };
        if !turn.open_messages.insert(item.to_string()) {
            return Ok(());
        }
        let model = self.model.clone();
        self.stream(json!({
            "type": "message_start",
            "message": {"id": item, "type": "message", "role": "assistant", "model": model, "content": []},
        }))
        .await?;
        self.stream(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "text", "text": ""},
        }))
        .await
    }

    async fn item_started(&mut self, item: &Value) -> anyhow::Result<()> {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        match item["type"].as_str() {
            Some("agentMessage") => self.open_message(&id).await,
            Some(_) if tool_use(item).is_some() => self.announce_tool(item).await,
            _ => Ok(()),
        }
    }

    async fn announce_tool(&mut self, item: &Value) -> anyhow::Result<()> {
        let Some(block) = tool_use(item) else {
            return Ok(());
        };
        let id = item["id"].as_str().unwrap_or_default().to_string();
        if let Some(turn) = self.turn.as_mut()
            && !turn.announced_tools.insert(id.clone())
        {
            return Ok(());
        }
        self.assistant(&id, json!([block]), None).await
    }

    async fn item_completed(&mut self, item: &Value) -> anyhow::Result<()> {
        let id = item["id"].as_str().unwrap_or_default().to_string();
        match item["type"].as_str() {
            Some("agentMessage") => {
                let text = item["text"].as_str().unwrap_or_default().to_string();
                let was_open = self
                    .turn
                    .as_mut()
                    .is_some_and(|turn| turn.open_messages.remove(&id));
                if was_open {
                    self.stream(json!({"type": "content_block_stop", "index": 0}))
                        .await?;
                    self.stream(json!({"type": "message_stop"})).await?;
                }
                if let Some(turn) = self.turn.as_mut() {
                    turn.last_text = Some(text.clone());
                    if item["phase"].as_str() == Some("final_answer") {
                        turn.final_text = Some(text.clone());
                    }
                }
                let usage = self.last_request_usage.as_ref().map(TokenSums::to_envelope);
                self.assistant(&id, json!([{"type": "text", "text": text}]), usage)
                    .await
            }
            Some("reasoning") => {
                let text = reasoning_text(item);
                if text.trim().is_empty() {
                    return Ok(());
                }
                self.assistant(
                    &id,
                    json!([{"type": "thinking", "thinking": text, "signature": ""}]),
                    None,
                )
                .await
            }
            Some(_) => {
                let Some((content, is_error)) = tool_result(item) else {
                    return Ok(());
                };
                self.announce_tool(item).await?;
                self.tool_result(&id, content, is_error).await
            }
            None => Ok(()),
        }
    }

    async fn finish_turn(&mut self, turn_object: &Value) -> anyhow::Result<()> {
        let Some(turn) = self.turn.take() else {
            return Ok(());
        };
        // The turn object carries its final message when no item event did.
        let from_items = turn_object["items"].as_array().and_then(|items| {
            items
                .iter()
                .rev()
                .find(|item| item["type"] == "agentMessage")
                .and_then(|item| item["text"].as_str())
                .map(str::to_string)
        });
        let text = turn
            .final_text
            .clone()
            .or(from_items)
            .or(turn.last_text.clone())
            .unwrap_or_default();
        let mut result = json!({
            "type": "result",
            "duration_ms": turn_object["durationMs"]
                .as_u64()
                .unwrap_or_else(|| turn.started.elapsed().as_millis() as u64),
            "num_turns": turn.model_requests.max(1),
            "usage": turn.usage.to_envelope(),
        });
        match turn_object["status"].as_str() {
            Some("interrupted") => {
                result["subtype"] = json!("interrupted");
                result["interrupted"] = json!(true);
                result["is_error"] = json!(false);
                result["result"] = json!(text);
            }
            Some("failed") => {
                let error = turn_object
                    .get("error")
                    .filter(|error| !error.is_null())
                    .or(turn.error.as_ref());
                let mut message = error
                    .and_then(|error| error["message"].as_str())
                    .unwrap_or("the codex turn failed")
                    .to_string();
                if let Some(details) = error.and_then(|error| error["additionalDetails"].as_str()) {
                    message.push_str(": ");
                    message.push_str(details);
                }
                result["subtype"] = json!("error");
                result["is_error"] = json!(true);
                result["result"] = json!(message);
                if let Some(status) =
                    error.and_then(|error| api_error_status(&error["codexErrorInfo"]))
                {
                    result["apiErrorStatus"] = json!(status);
                }
            }
            Some("completed") => {
                result["subtype"] = json!("success");
                result["is_error"] = json!(false);
                result["result"] = json!(text);
            }
            _ => anyhow::bail!("invalid terminal Codex turn status"),
        }
        if turn.compact && result["result"] == "" {
            result["result"] = json!("Compacted.");
        }
        self.emit(result).await?;
        self.start_next().await
    }

    /// The app-server exited mid-session: end the running turn with an error
    /// and stop.
    async fn server_gone(&mut self) -> anyhow::Result<()> {
        let message = "the codex app-server exited";
        warn(message);
        if self.turn.take().is_some() || !self.queue.is_empty() {
            self.queue.clear();
            self.error_result(message).await?;
        }
        Ok(())
    }
}

fn text_input(text: &str) -> Value {
    json!({"type": "text", "text": text, "text_elements": []})
}

fn reasoning_text(item: &Value) -> String {
    let join = |key: &str| {
        item[key]
            .as_array()
            .map(|parts| {
                parts
                    .iter()
                    .filter_map(|part| part.as_str().or_else(|| part["text"].as_str()))
                    .collect::<Vec<_>>()
                    .join("\n\n")
            })
            .unwrap_or_default()
    };
    let summary = join("summary");
    if summary.trim().is_empty() {
        join("content")
    } else {
        summary
    }
}

/// The `tool_use` block for a tool-running item, named the way the claude
/// CLI names the equivalent tool so transcript consumers classify it.
fn tool_use(item: &Value) -> Option<Value> {
    let id = item["id"].as_str().unwrap_or_default();
    let (name, input) = match item["type"].as_str()? {
        "commandExecution" => (
            "Bash".to_string(),
            json!({"command": item["command"], "cwd": item["cwd"]}),
        ),
        "fileChange" => (
            "apply_patch".to_string(),
            json!({
                "changes": item["changes"].as_array().map(|changes| changes.iter().map(|change| json!({
                    "path": change["path"],
                    "kind": change["kind"]["type"],
                    "diff": change["diff"],
                })).collect::<Vec<_>>()).unwrap_or_default(),
            }),
        ),
        "mcpToolCall" => (
            format!(
                "mcp__{}__{}",
                item["server"].as_str().unwrap_or_default(),
                item["tool"].as_str().unwrap_or_default()
            ),
            item.get("arguments")
                .cloned()
                .filter(|args| !args.is_null())
                .unwrap_or(json!({})),
        ),
        "webSearch" => ("WebSearch".to_string(), json!({"query": item["query"]})),
        _ => return None,
    };
    Some(json!({"type": "tool_use", "id": id, "name": name, "input": input}))
}

/// The `tool_result` content and error flag for a completed tool item.
fn tool_result(item: &Value) -> Option<(Value, bool)> {
    let status = item["status"].as_str().unwrap_or("completed");
    let failed = matches!(status, "failed" | "declined");
    match item["type"].as_str()? {
        "commandExecution" => {
            let mut output = item["aggregatedOutput"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let exit_code = item["exitCode"].as_i64();
            if let Some(code) = exit_code.filter(|code| *code != 0) {
                if !output.is_empty() && !output.ends_with('\n') {
                    output.push('\n');
                }
                output.push_str(&format!("exit code {code}"));
            }
            if status == "declined" {
                output = "command declined".to_string();
            }
            Some((
                json!(output),
                failed || exit_code.is_some_and(|code| code != 0),
            ))
        }
        "fileChange" => {
            let paths: Vec<String> = item["changes"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|change| {
                            format!(
                                "{} {}",
                                change["kind"]["type"].as_str().unwrap_or("update"),
                                change["path"].as_str().unwrap_or_default()
                            )
                        })
                        .collect()
                })
                .unwrap_or_default();
            let summary = if failed {
                format!("patch {status}: {}", paths.join(", "))
            } else {
                paths.join("\n")
            };
            Some((json!(summary), failed))
        }
        "mcpToolCall" => {
            if let Some(message) = item["error"]["message"].as_str() {
                return Some((json!(message), true));
            }
            let content = item["result"]["content"].clone();
            let content = if content.is_array() {
                content
            } else {
                json!("")
            };
            Some((content, failed))
        }
        "webSearch" => Some((
            json!(
                item["results"]
                    .as_array()
                    .map(|results| Value::Array(results.clone()).to_string())
                    .unwrap_or_else(|| format!(
                        "searched: {}",
                        item["query"].as_str().unwrap_or_default()
                    ))
            ),
            false,
        )),
        _ => None,
    }
}

/// The HTTP-ish status the daemon's disruption detection keys on, for the
/// failures that mean the account is rate limited or the backend overloaded.
fn api_error_status(info: &Value) -> Option<u64> {
    match info.as_str() {
        Some("rateLimitExceeded" | "usageLimitExceeded") => return Some(429),
        Some("serverOverloaded") => return Some(529),
        _ => {}
    }
    info.as_object()?
        .values()
        .find_map(|detail| detail["httpStatusCode"].as_u64())
        .filter(|status| matches!(status, 429 | 529))
}

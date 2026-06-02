//! OpenAI Responses transport — the modern OpenAI path (verified live against
//! the Codex/ChatGPT backend).
//!
//! Conversation is a flat `input[]` of items. User turns are
//! `{type:"message", role:"user", content:[{type:"input_text",text}]}`. Model
//! output items (message / function_call / reasoning) are echoed back
//! verbatim, then each call gets a `{type:"function_call_output", call_id,
//! output}`. Tools are flat (`{type:"function", name, description,
//! parameters, strict}`); server-side web search is `{type:"web_search"}`.
//!
//! Two auth modes:
//!   - API key (`OPENAI_API_KEY`) → `{base}/responses`, `Bearer` header.
//!   - ChatGPT OAuth (Codex `~/.codex/auth.json`) → the ChatGPT backend with
//!     `chatgpt-account-id` + `originator` headers. Used when no API key.
//!
//! The backend streams SSE; we read the full body and parse events (no
//! token-by-token emit needed — the daemon gets the assistant+result
//! envelope from the harness either way).
//!
//! Wire contract tracks the modern codex CLI (`openai/codex` `core/src/client.rs`)
//! rather than the frozen 2024 shape: a **stable `session-id`** header + a
//! **per-turn `thread-id`** (no random-per-request id), **no defunct
//! `OpenAI-Beta: responses=experimental`**, a stable **`prompt_cache_key`**,
//! **`service_tier`** for the `/fast`→`priority` lever, and reasoning continuity
//! via `include:["reasoning.encrypted_content"]` with encrypted reasoning items
//! replayed across turns. SSE reads carry a per-event idle timeout, and a `401`
//! triggers a one-shot token refresh + retry.

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiResponsesTransport {
    http: reqwest::Client,
    endpoint: String,
    auth: Auth,
    /// Stable per-session id (codex `session-id` header + `prompt_cache_key`).
    /// Set once via [`Transport::set_session_id`]; empty until then.
    session_id: String,
    /// Per-turn id (codex `thread-id` header). Regenerated on each new user
    /// turn (`push_user_text`), stable across the tool-call steps within it.
    thread_id: String,
    /// Flat Responses `input[]` buffer.
    input: Vec<Value>,
}

enum Auth {
    /// Standard OpenAI: `Authorization: Bearer <key>`.
    ApiKey(String),
    /// ChatGPT backend: bearer access token + account id.
    ChatGpt {
        access_token: String,
        account_id: String,
    },
}

impl OpenAiResponsesTransport {
    pub async fn from_env() -> Result<Self> {
        let http = reqwest::Client::new();
        if let Ok(key) = std::env::var("OPENAI_API_KEY") {
            let base = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
                .trim_end_matches('/')
                .to_string();
            return Ok(Self {
                http,
                endpoint: format!("{base}/responses"),
                auth: Auth::ApiKey(key),
                session_id: String::new(),
                thread_id: new_id(),
                input: Vec::new(),
            });
        }
        // Codex ChatGPT OAuth: load (refreshing if near expiry, cooperatively
        // with the Codex CLI via `~/.codex/auth.json`).
        let auth = super::codex_auth::load_fresh(&http)
            .await
            .context("no OPENAI_API_KEY and could not load/refresh Codex ChatGPT auth")?;
        let endpoint = std::env::var("OPENAI_RESPONSES_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/responses".to_string());
        Ok(Self {
            http,
            endpoint,
            auth: Auth::ChatGpt {
                access_token: auth.access_token,
                account_id: auth.account_id,
            },
            session_id: String::new(),
            thread_id: new_id(),
            input: Vec::new(),
        })
    }

    /// Attach the codex-style identity + auth headers shared by every request
    /// (the main turn, compaction). `session-id` is stable; `thread-id` is the
    /// current turn. `OpenAI-Beta: responses=experimental` is intentionally NOT
    /// sent — it is defunct in codex `main`.
    fn apply_headers(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let mut rb = rb
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .header("originator", originator())
            .header("user-agent", user_agent())
            .timeout(super::http::request_timeout());
        if !self.session_id.is_empty() {
            rb = rb.header("session-id", self.session_id.clone());
        }
        rb = rb.header("thread-id", self.thread_id.clone());
        rb = match &self.auth {
            Auth::ApiKey(k) => rb.header("authorization", format!("Bearer {k}")),
            Auth::ChatGpt {
                access_token,
                account_id,
            } => rb
                .header("authorization", format!("Bearer {access_token}"))
                .header("chatgpt-account-id", account_id.clone()),
        };
        rb
    }
}

#[async_trait]
impl Transport for OpenAiResponsesTransport {
    fn name(&self) -> &'static str {
        "openai-responses"
    }

    fn set_session_id(&mut self, id: String) {
        self.session_id = id;
    }

    fn push_user_text(&mut self, text: &str) {
        // A new user turn begins: rotate the per-turn `thread-id` (codex mints a
        // fresh ThreadId per turn while `session-id` stays stable).
        self.thread_id = new_id();
        self.input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }));
    }

    fn push_tool_results(&mut self, results: Vec<super::ToolResult>) {
        for r in results {
            self.input.push(json!({
                "type": "function_call_output",
                "call_id": r.id,
                "output": r.content,
            }));
        }
    }

    async fn run_turn(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn super::TurnSink,
    ) -> Result<TurnOutput> {
        let body = self.build_body(tools, opts);

        let resp = self
            .send_with_auth_recovery("openai-responses", &body)
            .await?;
        let status = resp.status();
        if !status.is_success() {
            let sse = resp.text().await.unwrap_or_default();
            anyhow::bail!(classify_http_error(status, &sse));
        }

        // Stream the SSE: forward text/reasoning deltas to the sink live (in
        // Anthropic shape) while accumulating the full body, then hand it to the
        // proven `parse_sse` for the authoritative item/usage reconstruction.
        let mut stream = resp.bytes_stream();
        let idle = super::http::stream_idle_timeout();
        let mut buf: Vec<u8> = Vec::new();
        let mut accum = String::new();
        let mut text_started = false;
        loop {
            // Bound the gap between events: a connection that stays open but
            // stops producing is a hung turn, not progress.
            let next = tokio::time::timeout(idle, stream.next())
                .await
                .context("responses SSE idle timeout (no event within idle window)")?;
            let Some(chunk) = next else { break };
            let chunk = chunk.context("read responses SSE chunk")?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line_cow = String::from_utf8_lossy(&raw);
                // Keep the full SSE text (with newline) for parse_sse.
                accum.push_str(&line_cow);
                let line = line_cow.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                match ev["type"].as_str().unwrap_or("") {
                    "response.output_text.delta" => {
                        if let Some(t) = ev["delta"].as_str()
                            && !t.is_empty()
                        {
                            if !text_started {
                                sink.stream_event(json!({
                                    "type": "content_block_start",
                                    "index": 0,
                                    "content_block": {"type": "text", "text": ""},
                                }));
                                text_started = true;
                            }
                            sink.stream_event(json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "text_delta", "text": t},
                            }));
                        }
                    }
                    "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                        if let Some(t) = ev["delta"].as_str()
                            && !t.is_empty()
                        {
                            sink.stream_event(json!({
                                "type": "content_block_delta",
                                "index": 0,
                                "delta": {"type": "thinking_delta", "thinking": t},
                            }));
                        }
                    }
                    _ => {}
                }
            }
        }
        self.parse_sse(&accum)
    }

    fn snapshot(&self) -> Value {
        json!(self.input)
    }
    fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.input = arr.clone();
        }
    }

    async fn compact(
        &mut self,
        keep_tail: usize,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        let n = self.input.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        let limit = n.saturating_sub(keep_tail);
        let Some(split) = responses_split(&self.input, limit) else {
            return Ok(None);
        };
        let transcript = render_responses_transcript(&self.input[..split]);
        let summary = self.summarize_text(&transcript, instruction, opts).await?;
        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": format!("[Earlier conversation compacted to a summary]\n\n{summary}")}],
        }));
        rebuilt.extend_from_slice(&self.input[split..]);
        self.input = rebuilt;
        Ok(Some(summary))
    }
}

impl OpenAiResponsesTransport {
    /// Build the Responses request body (pure; no I/O), so the system split
    /// (stable `instructions` + trailing volatile `developer` item that is
    /// never persisted into self.input) is unit-testable.
    fn build_body(&self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Value {
        let mut tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                    "strict": false,
                })
            })
            .collect();
        if opts.web_search {
            tool_defs.push(json!({"type": "web_search"}));
        }

        // The cache-stable prefix goes in `instructions` (cached via
        // prompt_cache_key); the volatile tail (manifest/nudges) rides as a
        // trailing `developer` input item, appended per-request only so it
        // never persists into self.input and can't disturb the cached prefix.
        let mut input = self.input.clone();
        if let Some(volatile) = opts.system.volatile_text() {
            input.push(json!({
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": volatile}],
            }));
        }
        let mut body = json!({
            "model": opts.model,
            "input": input,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "stream": true,
            "store": false,
        });
        // The ChatGPT backend rejects an empty/missing `instructions` field
        // ("Instructions are required"); always send a non-empty value.
        let instructions = opts
            .system
            .stable_text()
            .unwrap_or("You are a helpful coding assistant operating non-interactively.");
        body["instructions"] = json!(instructions);
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
        }
        // Stable cache key (codex uses the thread id): keeps the cached prefix
        // pinned to this session instead of relying on implicit server keying.
        if !self.session_id.is_empty() {
            body["prompt_cache_key"] = json!(self.session_id);
        }
        // `/fast` lever: forward the priority/flex tier when set (and not the
        // literal "default", which the backend rejects as a no-op).
        if let Some(tier) = service_tier_for_request(opts.service_tier.as_deref()) {
            body["service_tier"] = json!(tier);
        }
        // Reasoning: only for reasoning-capable models, and only when an effort
        // was requested. Bundle the summary (codex default `auto`) and request
        // encrypted reasoning so reasoning items can be replayed across turns
        // under `store:false` (see `parse_sse`).
        if let Some(e) = &opts.effort {
            if model_supports_reasoning(&opts.model) {
                let mut reasoning = json!({ "effort": normalize_effort(e) });
                if let Some(summary) = reasoning_summary() {
                    reasoning["summary"] = json!(summary);
                }
                body["reasoning"] = reasoning;
                body["include"] = json!(["reasoning.encrypted_content"]);
            } else {
                tracing::warn!(
                    model = %opts.model,
                    "effort requested but model is not reasoning-capable; omitting reasoning"
                );
            }
        }
        body
    }

    /// Send the request with transport retry, and — for the ChatGPT-OAuth arm —
    /// recover from a single `401` by force-refreshing the codex token and
    /// retrying once. Mirrors codex's reload→refresh `UnauthorizedRecovery`.
    async fn send_with_auth_recovery(
        &mut self,
        label: &str,
        body: &Value,
    ) -> Result<reqwest::Response> {
        let resp = super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.endpoint)).json(body).send()
        })
        .await
        .context("responses request")?;
        if resp.status() != reqwest::StatusCode::UNAUTHORIZED
            || !matches!(self.auth, Auth::ChatGpt { .. })
        {
            return Ok(resp);
        }
        tracing::warn!("responses 401; force-refreshing codex token and retrying once");
        let fresh = super::codex_auth::force_refresh(&self.http)
            .await
            .context("responses 401; codex token refresh failed")?;
        self.auth = Auth::ChatGpt {
            access_token: fresh.access_token,
            account_id: fresh.account_id,
        };
        super::http::send_with_retry(label, || {
            self.apply_headers(self.http.post(&self.endpoint)).json(body).send()
        })
        .await
        .context("responses request (after token refresh)")
    }

    /// Parse the SSE body: accumulate completed output items, append them to
    /// the input buffer (so the next turn carries context), and normalize.
    fn parse_sse(&mut self, sse: &str) -> Result<TurnOutput> {
        let mut output_items: Vec<Value> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;

        for line in sse.lines() {
            let line = line.trim();
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                continue;
            };
            match ev["type"].as_str().unwrap_or("") {
                "response.output_item.done" => {
                    if let Some(item) = ev.get("item") {
                        output_items.push(item.clone());
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let r = &ev["response"];
                    // OpenAI Responses `input_tokens` is cache-INCLUSIVE; the
                    // cached subset lives in `input_tokens_details.cached_tokens`.
                    // Subtract it so `input_tokens` stays fresh.
                    let total_input = r["usage"]["input_tokens"].as_u64().unwrap_or(0);
                    let cached = r["usage"]["input_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    usage = Usage {
                        input_tokens: total_input.saturating_sub(cached),
                        output_tokens: r["usage"]["output_tokens"].as_u64().unwrap_or(0),
                        cached_input_tokens: cached,
                        cache_creation_input_tokens: 0,
                    };
                    if r["status"].as_str() == Some("incomplete") {
                        stop = StopReason::Length;
                        // Otherwise-silent path: the model stopped short (e.g.
                        // max_output_tokens, content filter). Surface the reason
                        // so a spurious-stop turn is diagnosable from the log.
                        tracing::warn!(
                            reason = %r["incomplete_details"]["reason"]
                                .as_str()
                                .unwrap_or("unknown"),
                            "responses turn incomplete; stopping short"
                        );
                    }
                }
                "response.failed" | "error" => {
                    let err = if ev["type"] == "response.failed" {
                        &ev["response"]["error"]
                    } else {
                        &ev["error"]
                    };
                    let code = err["code"]
                        .as_str()
                        .or_else(|| ev["code"].as_str())
                        .unwrap_or("");
                    let message = err["message"]
                        .as_str()
                        .or_else(|| ev["message"].as_str())
                        .unwrap_or(data);
                    anyhow::bail!(classify_stream_error(code, message));
                }
                _ => {}
            }
        }

        // Echo the model's output items back into the buffer for continuity.
        // Reasoning items need care under `store:false` (required by the ChatGPT
        // backend): a reasoning item replayed *by reference* (`rs_…` with no
        // payload) 404s ("Item with id … not found") because it isn't persisted
        // server-side. But because we request `include:["reasoning.encrypted_content"]`,
        // reasoning items come back carrying `encrypted_content` — self-contained
        // and safe to replay, preserving cross-turn reasoning continuity. So:
        // keep reasoning items that carry `encrypted_content`; drop the rest;
        // keep every non-reasoning item.
        self.input.extend(
            output_items
                .iter()
                .filter(|item| {
                    item["type"].as_str() != Some("reasoning")
                        || item.get("encrypted_content").and_then(Value::as_str).is_some()
                })
                .cloned(),
        );

        let mut text = String::new();
        let mut thinking = String::new();
        let mut tool_calls = Vec::new();
        for item in &output_items {
            match item["type"].as_str().unwrap_or("") {
                "message" => {
                    if let Some(parts) = item["content"].as_array() {
                        for p in parts {
                            if p["type"] == "output_text"
                                && let Some(t) = p["text"].as_str()
                            {
                                text.push_str(t);
                            }
                        }
                    }
                }
                // Reasoning items carry summary/content text — surface for
                // display only (not replayed; `store:false` drops them server-side).
                "reasoning" => {
                    for key in ["summary", "content"] {
                        if let Some(parts) = item[key].as_array() {
                            for p in parts {
                                if let Some(t) = p["text"].as_str() {
                                    thinking.push_str(t);
                                }
                            }
                        }
                    }
                }
                "function_call" => {
                    let args_str = item["arguments"].as_str().unwrap_or("{}");
                    if let (Some(call_id), Some(name)) =
                        (item["call_id"].as_str(), item["name"].as_str())
                    {
                        tool_calls.push(super::ToolCall {
                            id: call_id.to_string(),
                            name: name.to_string(),
                            args: serde_json::from_str(args_str).unwrap_or(json!({})),
                        });
                    }
                }
                _ => {} // reasoning / other items carried in buffer, not surfaced
            }
        }

        if !tool_calls.is_empty() {
            stop = StopReason::ToolCalls;
        }

        Ok(TurnOutput {
            text,
            thinking,
            tool_calls,
            stop,
            usage,
        })
    }

    /// One-shot summarization over `transcript` for compaction. Streams the
    /// response and concatenates `output_text` deltas. Does NOT touch the input
    /// buffer — the caller swaps it afterward.
    async fn summarize_text(
        &self,
        transcript: &str,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<String> {
        let body = json!({
            "model": opts.model,
            "input": [{
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": format!("{transcript}\n\n---\n{instruction}")}],
            }],
            "instructions": "You summarize coding-agent conversations precisely and completely.",
            "stream": true,
            "store": false,
        });
        let resp = super::http::send_with_retry("openai-responses/compact", || {
            self.apply_headers(self.http.post(&self.endpoint))
                .json(&body)
                .send()
        })
        .await
        .context("responses compaction request")?;
        let status = resp.status();
        if !status.is_success() {
            let t = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai responses compact {status}: {t}");
        }
        let mut out = String::new();
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read responses compact chunk")?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    continue;
                };
                if ev["type"].as_str() == Some("response.output_text.delta")
                    && let Some(t) = ev["delta"].as_str()
                {
                    out.push_str(t);
                }
            }
        }
        if out.trim().is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(out)
    }
}

/// Largest split `≤ limit` whose kept tail `[split..]` is a valid standalone
/// Responses input — no `function_call_output` orphaned from the
/// `function_call` (matched by `call_id`) it answers. `None` if none exists.
fn responses_split(input: &[Value], limit: usize) -> Option<usize> {
    (1..limit).rev().find(|&s| {
        let tail = &input[s..];
        let calls: std::collections::HashSet<&str> = tail
            .iter()
            .filter(|it| it["type"] == "function_call")
            .filter_map(|it| it["call_id"].as_str())
            .collect();
        !tail.iter().any(|it| {
            it["type"] == "function_call_output"
                && it["call_id"].as_str().is_some_and(|c| !calls.contains(c))
        })
    })
}

/// Render a slice of the Responses input buffer to a plain-text transcript for
/// summarization. Tool outputs are truncated to keep the prompt bounded.
fn render_responses_transcript(items: &[Value]) -> String {
    let mut s = String::new();
    for it in items {
        match it["type"].as_str().unwrap_or("") {
            "message" => {
                let role = it["role"].as_str().unwrap_or("?");
                s.push_str(&format!("\n## {role}\n"));
                if let Some(parts) = it["content"].as_array() {
                    for p in parts {
                        if let Some(t) = p["text"].as_str() {
                            s.push_str(t);
                        }
                    }
                }
                s.push('\n');
            }
            "function_call" => s.push_str(&format!(
                "\n## assistant\n[tool_call {} {}]\n",
                it["name"].as_str().unwrap_or(""),
                it["arguments"].as_str().unwrap_or("")
            )),
            "function_call_output" => s.push_str(&format!(
                "\n## tool\n[tool_result {}]\n",
                super::truncate(it["output"].as_str().unwrap_or(""), 2000)
            )),
            _ => {}
        }
    }
    s
}

/// Map an effort token onto codex's `ReasoningEffort` range
/// (`none/minimal/low/medium/high/xhigh`). `max` stays conservative at `high`
/// (universally supported); callers wanting `xhigh` (newer, model-specific)
/// pass it explicitly.
fn normalize_effort(e: &str) -> &'static str {
    match e.trim().to_ascii_lowercase().as_str() {
        "none" => "none",
        "minimal" | "min" => "minimal",
        "low" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "xhigh" | "x-high" | "extra-high" => "xhigh",
        "max" => "high",
        _ => "medium",
    }
}

/// Fresh random id for the `session-id`/`thread-id` headers.
fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Request originator — codex's first-party value by default so the ChatGPT
/// backend routes/accounts the request as it expects. Overridable to match
/// codex's own `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, or `BRO_HARNESS_ORIGINATOR`.
fn originator() -> String {
    std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        .or_else(|_| std::env::var("BRO_HARNESS_ORIGINATOR"))
        .unwrap_or_else(|_| "codex_cli_rs".to_string())
}

/// Descriptive `User-Agent` in codex's shape (`<originator>/<ver> (<os>; <arch>)`),
/// fully overridable via `BRO_HARNESS_USER_AGENT`.
fn user_agent() -> String {
    std::env::var("BRO_HARNESS_USER_AGENT").unwrap_or_else(|_| {
        format!(
            "{}/{} ({}; {})",
            originator(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })
}

/// True unless the model is a known non-reasoning family. Codex gates reasoning
/// on a per-model capability flag from its catalog; lacking that catalog we
/// fail open (unknown ⇒ reasoning allowed, since the ChatGPT backend only ever
/// serves reasoning models) but suppress the obvious GPT-3/4 families so an
/// effort value can't 400 a non-reasoning model.
fn model_supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    const NON_REASONING_PREFIXES: &[&str] = &["gpt-4", "gpt-3", "chatgpt-4"];
    !NON_REASONING_PREFIXES.iter().any(|p| m.starts_with(p))
}

/// Reasoning summary mode (codex default `auto`). `BRO_HARNESS_REASONING_SUMMARY`
/// overrides; `none`/`off`/empty omits the field.
fn reasoning_summary() -> Option<String> {
    match std::env::var("BRO_HARNESS_REASONING_SUMMARY") {
        Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "none" | "off" | "") => None,
        Ok(v) => Some(v.trim().to_string()),
        Err(_) => Some("auto".to_string()),
    }
}

/// Normalize a requested service tier: forward it unless it's empty or the
/// literal `"default"` (which the backend rejects as a no-op). Codex's
/// `service_tier_for_request` does the same drop.
fn service_tier_for_request(tier: Option<&str>) -> Option<String> {
    let t = tier?.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("default") {
        return None;
    }
    Some(t.to_string())
}

/// Classify a Responses stream error (`response.failed` / `error`) into a clear,
/// actionable message. Mirrors codex's error-code mapping
/// (`codex-api/src/sse/responses.rs`).
fn classify_stream_error(code: &str, message: &str) -> String {
    match code {
        "context_length_exceeded" | "context_window_exceeded" => {
            format!("context window exceeded ({message}); compact the conversation and retry")
        }
        "insufficient_quota" | "usage_not_included" => {
            format!("quota/usage limit [{code}]: {message}")
        }
        "server_is_overloaded" | "slow_down" => format!("server overloaded [{code}]: {message}"),
        "" => format!("responses stream error: {message}"),
        _ => format!("responses stream error [{code}]: {message}"),
    }
}

/// Classify a non-2xx HTTP response, surfacing any error code from the body
/// envelope so failures are diagnosable from the log.
fn classify_http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["code"]
                .as_str()
                .or_else(|| v["error"]["type"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if code.is_empty() {
        format!("openai responses {status}: {body}")
    } else {
        format!("openai responses {status} [{code}]: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;

    fn transport() -> OpenAiResponsesTransport {
        OpenAiResponsesTransport {
            http: reqwest::Client::new(),
            endpoint: "http://x".into(),
            auth: Auth::ApiKey("k".into()),
            session_id: "sess-1".into(),
            thread_id: "thread-1".into(),
            input: vec![json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hi"}],
            })],
        }
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    #[test]
    fn stable_is_instructions_volatile_is_trailing_developer_item() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("MANIFEST".into()),
            }),
        );
        // Stable → cached instructions.
        assert_eq!(body["instructions"], "BASE");
        // Volatile → trailing developer item, appended after the conversation.
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "MANIFEST");
        // Volatile never persisted into self.input.
        assert_eq!(transport().input.len(), 1);
    }

    #[test]
    fn empty_stable_falls_back_to_nonempty_instructions() {
        // The ChatGPT backend rejects empty instructions; the fallback must hold
        // even when only a volatile tail is present.
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: None,
                volatile: Some("V".into()),
            }),
        );
        assert!(!body["instructions"].as_str().unwrap().is_empty());
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[1]["role"], "developer");
    }

    #[test]
    fn responses_split_avoids_orphan_tool_output() {
        let input = vec![
            json!({"type": "message", "role": "user", "content": []}),
            json!({"type": "function_call", "call_id": "a", "name": "f", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "a", "output": "r"}),
            json!({"type": "message", "role": "assistant", "content": []}),
        ];
        // split=2 would orphan function_call_output(a) (its call at index 1 is
        // discarded); split=1 keeps the call/output pair together.
        assert_eq!(responses_split(&input, 3), Some(1));
        // Cutting only the final item is never offered (limit 1 → empty range).
        assert_eq!(responses_split(&input, 1), None);
    }

    #[test]
    fn modern_body_carries_cache_key_service_tier_and_reasoning() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.effort = Some("medium".into());
        o.service_tier = Some("priority".into());
        let body = transport().build_body(&[], &o);
        // No defunct OpenAI-Beta on the body; store stays false; stream true.
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        // Stable cache key = session id.
        assert_eq!(body["prompt_cache_key"], "sess-1");
        // /fast lever forwarded.
        assert_eq!(body["service_tier"], "priority");
        // Reasoning sent for a reasoning-capable model, with encrypted include.
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn default_service_tier_is_dropped() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.service_tier = Some("default".into());
        let body = transport().build_body(&[], &o);
        assert!(body.get("service_tier").is_none());
    }

    #[test]
    fn reasoning_omitted_for_non_reasoning_model() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.model = "gpt-4o".into();
        o.effort = Some("high".into());
        let body = transport().build_body(&[], &o);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn pure_helpers() {
        assert_eq!(normalize_effort("minimal"), "minimal");
        assert_eq!(normalize_effort("max"), "high");
        assert_eq!(normalize_effort("xhigh"), "xhigh");
        assert_eq!(normalize_effort("bogus"), "medium");
        assert!(model_supports_reasoning("gpt-5-codex"));
        assert!(model_supports_reasoning("o3"));
        assert!(!model_supports_reasoning("gpt-4o"));
        assert!(!model_supports_reasoning("gpt-4.1"));
        assert_eq!(service_tier_for_request(Some("priority")).as_deref(), Some("priority"));
        assert_eq!(service_tier_for_request(Some("default")), None);
        assert_eq!(service_tier_for_request(Some("")), None);
        assert_eq!(service_tier_for_request(None), None);
    }

    #[test]
    fn reasoning_item_replayed_only_with_encrypted_content() {
        let mut tx = transport();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"ENC\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"bare\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        tx.parse_sse(sse).unwrap();
        // Buffer keeps: original user msg, the encrypted reasoning item, the
        // message — but NOT the bare reasoning item (would 404 on replay).
        let reasoning: Vec<_> = tx
            .input
            .iter()
            .filter(|i| i["type"] == "reasoning")
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["encrypted_content"], "ENC");
    }

    #[test]
    fn classify_stream_error_names_codes() {
        assert!(classify_stream_error("context_length_exceeded", "too big").contains("context window"));
        assert!(classify_stream_error("server_is_overloaded", "busy").contains("overloaded"));
        assert!(classify_stream_error("", "boom").contains("boom"));
    }

    #[test]
    fn parse_sse_surfaces_reasoning_as_thinking() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"pondering\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        let out = transport().parse_sse(sse).unwrap();
        assert_eq!(out.text, "answer");
        // Reasoning surfaced for display…
        assert_eq!(out.thinking, "pondering");
    }
}

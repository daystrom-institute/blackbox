//! Anthropic Messages transport. Clean request body — none of the
//! schema-violating CLI scaffolding that broke GLM/DeepSeek. Responses parsed
//! loosely (content blocks kept raw) so provider drift never breaks decoding.
//!
//! Streaming: the turn is requested with `"stream": true` and the SSE event
//! stream is parsed incrementally. Each parsed Anthropic event is forwarded
//! verbatim to the [`TurnSink`] (the harness wraps it as a Claude
//! `stream_event` NDJSON line, which the daemon already consumes) while the
//! transport simultaneously folds the deltas into the normalized
//! [`TurnOutput`] and the replay buffer.

use super::{StopReason, Transport, TurnOpts, TurnOutput, TurnSink, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct AnthropicTransport {
    http: reqwest::Client,
    base_url: String,
    auth: Auth,
    version: String,
    /// Native conversation: `[{"role":..,"content":[blocks]}]`.
    messages: Vec<Value>,
}

enum Auth {
    Bearer(String),
    ApiKey(String),
}

impl AnthropicTransport {
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("ANTHROPIC_BASE_URL")
            .context("ANTHROPIC_BASE_URL not set")?
            .trim_end_matches('/')
            .to_string();
        let auth = if let Ok(t) = std::env::var("ANTHROPIC_AUTH_TOKEN") {
            Auth::Bearer(t)
        } else if let Ok(k) = std::env::var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(k)
        } else {
            anyhow::bail!("neither ANTHROPIC_AUTH_TOKEN nor ANTHROPIC_API_KEY set");
        };
        let version =
            std::env::var("ANTHROPIC_VERSION").unwrap_or_else(|_| "2023-06-01".to_string());
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            auth,
            version,
            messages: Vec::new(),
        })
    }

    /// Build the Messages request body (pure; no I/O), so the wire shape —
    /// notably the system-block cache-control placement — is unit-testable.
    fn build_body(&self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Value {
        let mut tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.schema,
                })
            })
            .collect();
        if opts.web_search {
            // Server-side; provider executes it and returns results inline.
            tool_defs.push(json!({
                "type": "web_search_20250305", "name": "web_search", "max_uses": 5,
            }));
        }

        let mut body = json!({
            "model": opts.model,
            "max_tokens": opts.max_tokens,
            "messages": messages_with_cache_breakpoint(&self.messages),
            "stream": true,
        });
        // System is up to two blocks: a cache-stable prefix (carries the
        // ephemeral cache breakpoint) and a volatile tail (manifest/nudges,
        // never cached, so a changing tail can't invalidate the cached prefix).
        let mut system_blocks: Vec<Value> = Vec::new();
        if let Some(stable) = opts.system.stable_text() {
            system_blocks.push(json!({
                "type": "text", "text": stable, "cache_control": cache_control(),
            }));
        }
        if let Some(volatile) = opts.system.volatile_text() {
            system_blocks.push(json!({ "type": "text", "text": volatile }));
        }
        if !system_blocks.is_empty() {
            body["system"] = json!(system_blocks);
        }
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
        }
        // Reasoning: mirror the Claude Code wire shape for these Anthropic-
        // compatible endpoints (glm/deepseek). Thinking budget is server-managed
        // (`adaptive`) rather than a fixed `budget_tokens`, which previously
        // starved output when budget >= max_tokens and produced empty,
        // spurious-stop turns. Effort is the categorical `output_config.effort`
        // knob (gated by the `effort` beta in the request header). Only emitted
        // when an effort is requested, preserving "no effort ⇒ no thinking".
        if let Some(effort) = opts.effort.as_deref() {
            body["thinking"] = json!({"type": "adaptive"});
            body["output_config"] = json!({"effort": effort});
        }
        body
    }

    /// One-shot, non-streaming summarization over `transcript`. Does NOT touch
    /// the conversation buffer — the caller swaps the buffer afterward.
    async fn summarize_text(
        &self,
        transcript: &str,
        instruction: &str,
        max_tokens: u32,
        opts: &TurnOpts,
    ) -> Result<String> {
        let body = json!({
            "model": opts.model,
            "max_tokens": max_tokens,
            "stream": false,
            "system": "You summarize coding-agent conversations precisely and completely.",
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": format!("{transcript}\n\n---\n{instruction}")}],
            }],
        });
        let url = format!("{}/v1/messages", self.base_url);
        let resp = super::http::send_with_retry("anthropic/compact", || {
            let mut rb = self
                .http
                .post(&url)
                .header("content-type", "application/json")
                .header("anthropic-version", &self.version)
                .timeout(super::http::request_timeout());
            rb = match &self.auth {
                Auth::Bearer(t) => rb.header("authorization", format!("Bearer {t}")),
                Auth::ApiKey(k) => rb.header("x-api-key", k.clone()),
            };
            rb.json(&body).send()
        })
        .await
        .context("compaction summarize request")?;
        let status = resp.status();
        let text = resp.text().await.context("read summarize body")?;
        if !status.is_success() {
            anyhow::bail!("anthropic compact {status}: {text}");
        }
        let v: Value = serde_json::from_str(&text).context("parse summarize response")?;
        let out = v["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        // Keep only the durable `<summary>` block, dropping the `<analysis>`
        // scratchpad the structured prompt asks for.
        let summary = super::extract_summary(&out);
        if summary.is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(summary)
    }
}

/// One in-progress content block accumulated from SSE deltas.
#[derive(Default)]
struct SseBlock {
    /// `"text"`, `"thinking"`, `"tool_use"`, `"server_tool_use"`, or a
    /// server-produced result block (`"tool_result"` / `"web_search_tool_result"`).
    kind: String,
    /// Accumulated `text_delta.text` or `thinking_delta.thinking`.
    text: String,
    tool_id: String,
    tool_name: String,
    /// Accumulated `input_json_delta.partial_json` (tool_use + server_tool_use).
    tool_json: String,
    /// Raw `content_block` captured verbatim at `content_block_start` for
    /// server-produced result blocks, whose content arrives inline there (no
    /// deltas) — kept so the server-tool turn replays faithfully and a paused
    /// turn can be resumed.
    raw: Option<Value>,
}

fn map_stop(reason: &str) -> StopReason {
    match reason {
        "tool_use" => StopReason::ToolCalls,
        "end_turn" | "stop_sequence" => StopReason::Done,
        "max_tokens" => StopReason::Length,
        other => StopReason::Other(other.to_string()),
    }
}

/// Fold one raw Anthropic SSE event into the running accumulators.
/// True when an in-band SSE `error` event is a transient provider failure worth
/// retrying the turn for (overload, rate limit, network/server hiccup) rather
/// than a permanent error (bad request, auth).
fn inband_error_retryable(ev: &Value) -> bool {
    let ty = ev["error"]["type"].as_str().unwrap_or("");
    if matches!(
        ty,
        "overloaded_error" | "rate_limit_error" | "api_error" | "timeout_error"
    ) {
        return true;
    }
    let m = ev["error"]["message"]
        .as_str()
        .unwrap_or("")
        .to_ascii_lowercase();
    m.contains("network error") || m.contains("overloaded") || m.contains("try again")
}

fn fold_sse(ev: &Value, blocks: &mut Vec<SseBlock>, usage: &mut Usage, stop: &mut StopReason) {
    let ensure = |blocks: &mut Vec<SseBlock>, idx: usize| {
        while blocks.len() <= idx {
            blocks.push(SseBlock::default());
        }
    };
    match ev["type"].as_str() {
        Some("message_start") => {
            let u = &ev["message"]["usage"];
            usage.input_tokens = u["input_tokens"].as_u64().unwrap_or(usage.input_tokens);
            usage.cached_input_tokens = u["cache_read_input_tokens"]
                .as_u64()
                .unwrap_or(usage.cached_input_tokens);
            usage.cache_creation_input_tokens = u["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(usage.cache_creation_input_tokens);
            if let Some(ot) = u["output_tokens"].as_u64() {
                usage.output_tokens = ot;
            }
        }
        Some("content_block_start") => {
            let idx = ev["index"].as_u64().unwrap_or(0) as usize;
            ensure(blocks, idx);
            let cb = &ev["content_block"];
            let b = &mut blocks[idx];
            b.kind = cb["type"].as_str().unwrap_or("").to_string();
            match b.kind.as_str() {
                // Both stream their input via `input_json_delta`.
                "tool_use" | "server_tool_use" => {
                    b.tool_id = cb["id"].as_str().unwrap_or("").to_string();
                    b.tool_name = cb["name"].as_str().unwrap_or("").to_string();
                }
                "text" | "thinking" => {}
                // Server-produced result block (e.g. `tool_result` /
                // `web_search_tool_result`): its content is inline in
                // `content_block_start`, so capture it verbatim for replay.
                _ => b.raw = Some(cb.clone()),
            }
        }
        Some("content_block_delta") => {
            let idx = ev["index"].as_u64().unwrap_or(0) as usize;
            ensure(blocks, idx);
            let d = &ev["delta"];
            match d["type"].as_str() {
                Some("text_delta") => {
                    blocks[idx].text.push_str(d["text"].as_str().unwrap_or(""));
                }
                Some("thinking_delta") => {
                    blocks[idx]
                        .text
                        .push_str(d["thinking"].as_str().unwrap_or(""));
                }
                Some("input_json_delta") => {
                    blocks[idx]
                        .tool_json
                        .push_str(d["partial_json"].as_str().unwrap_or(""));
                }
                _ => {}
            }
        }
        Some("message_delta") => {
            if let Some(sr) = ev["delta"]["stop_reason"].as_str() {
                *stop = map_stop(sr);
            }
            if let Some(ot) = ev["usage"]["output_tokens"].as_u64() {
                usage.output_tokens = ot;
            }
        }
        _ => {}
    }
}

/// Render a slice of the native message buffer to a plain-text transcript for
/// summarization. Tool I/O is rendered compactly and large tool results are
/// truncated so the summarization prompt stays bounded.
/// Max `pause_turn` resumes for a single server-tool turn before we stop and
/// return what we have. A safety bound against a pathological pause loop; a real
/// server tool (e.g. web_search with a small `max_uses`) pauses zero or one times.
const MAX_PAUSE_RESUMES: u32 = 8;

fn parse_tool_input(tool_json: &str) -> Value {
    serde_json::from_str(if tool_json.is_empty() { "{}" } else { tool_json })
        .unwrap_or_else(|_| json!({}))
}

/// Reconstruct one assistant segment from the SSE accumulators: the `content`
/// blocks to store for replay, plus the normalized text/thinking/tool-call
/// outputs. Server-side tool blocks (`server_tool_use` and the
/// `tool_result`/`web_search_tool_result` they produce) are preserved verbatim
/// into `content` so the turn replays faithfully and a paused turn can be
/// resumed — but they are NOT surfaced as client `tool_calls` (the server
/// already executed them). Thinking is returned for display only and never
/// enters `content` (Anthropic needs a persisted signature to replay it).
fn reconstruct_segment(blocks: &[SseBlock]) -> (Vec<Value>, String, String, Vec<super::ToolCall>) {
    let mut content: Vec<Value> = Vec::new();
    let mut text_out = String::new();
    let mut thinking_out = String::new();
    let mut tool_calls: Vec<super::ToolCall> = Vec::new();
    for b in blocks {
        match b.kind.as_str() {
            "text" if !b.text.is_empty() => {
                content.push(json!({"type": "text", "text": b.text}));
                text_out.push_str(&b.text);
            }
            "thinking" if !b.text.is_empty() => {
                thinking_out.push_str(&b.text);
            }
            "tool_use" => {
                let args = parse_tool_input(&b.tool_json);
                content.push(json!({
                    "type": "tool_use", "id": b.tool_id, "name": b.tool_name, "input": args.clone(),
                }));
                tool_calls.push(super::ToolCall {
                    id: b.tool_id.clone(),
                    name: b.tool_name.clone(),
                    args,
                });
            }
            "server_tool_use" => {
                // Server-executed: preserve for replay, do not dispatch.
                content.push(json!({
                    "type": "server_tool_use",
                    "id": b.tool_id,
                    "name": b.tool_name,
                    "input": parse_tool_input(&b.tool_json),
                }));
            }
            _ => {
                // Server-produced result block captured verbatim.
                if let Some(raw) = &b.raw {
                    content.push(raw.clone());
                }
            }
        }
    }
    (content, text_out, thinking_out, tool_calls)
}

fn render_transcript(messages: &[Value], tool_cap: usize) -> String {
    let mut s = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("?");
        s.push_str(&format!("\n## {role}\n"));
        match &m["content"] {
            Value::String(t) => s.push_str(t),
            Value::Array(blocks) => {
                for b in blocks {
                    match b["type"].as_str() {
                        Some("text") => s.push_str(b["text"].as_str().unwrap_or("")),
                        Some("tool_use") => s.push_str(&format!(
                            "[tool_use {} {}]",
                            b["name"].as_str().unwrap_or(""),
                            b["input"]
                        )),
                        Some("tool_result") => s.push_str(&format!(
                            "[tool_result {}]",
                            truncate(b["content"].as_str().unwrap_or(""), tool_cap)
                        )),
                        _ => {}
                    }
                    s.push('\n');
                }
            }
            _ => {}
        }
    }
    s
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let end = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    format!("{}… [truncated]", &s[..end])
}

#[async_trait]
impl Transport for AnthropicTransport {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn push_user_text(&mut self, text: &str) {
        self.messages.push(json!({
            "role": "user",
            "content": [{"type": "text", "text": text}],
        }));
    }

    fn push_tool_results(&mut self, results: Vec<super::ToolResult>) {
        let blocks: Vec<Value> = results
            .into_iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": r.content,
                    "is_error": r.is_error,
                })
            })
            .collect();
        self.messages
            .push(json!({"role": "user", "content": blocks}));
    }

    async fn run_turn(
        &mut self,
        tools: &[super::ToolSpec],
        opts: &TurnOpts,
        sink: &dyn TurnSink,
    ) -> Result<TurnOutput> {
        // `?beta=true` + the `anthropic-beta` header gate adaptive-thinking
        // effort and the 1M window on these endpoints (mirrors Claude Code).
        let url = format!("{}/v1/messages?beta=true", self.base_url);
        let betas = anthropic_betas();
        let max_inband = super::http::max_retries();
        let idle = super::http::stream_idle_timeout();

        // A server-tool turn can be split across `pause_turn` boundaries: the
        // server pauses a long server-side tool turn (e.g. web_search hitting its
        // iteration limit) and we resume by re-sending the conversation with the
        // partial assistant appended — the server continues where it left off.
        // Every segment is merged into ONE assistant message so the buffer stays
        // alternation-valid for the next turn; bounded so a pause loop can't run
        // away. `inband_attempt` resets per segment; `resumes` counts pauses.
        let mut acc_text = String::new();
        let mut acc_thinking = String::new();
        let mut acc_tool_calls: Vec<super::ToolCall> = Vec::new();
        let mut acc_usage = Usage::default();
        let mut assistant_idx: Option<usize> = None;
        let mut inband_attempt = 0u32;
        let mut resumes = 0u32;
        loop {
            inband_attempt += 1;
            // Rebuilt each iteration: on a resume the buffer now ends with the
            // partial assistant turn, so the request continues it (on an in-band
            // retry the buffer is unchanged, so the body is identical).
            let body = self.build_body(tools, opts);
            let resp = super::http::send_with_retry("anthropic/messages", || {
                let mut rb = self
                    .http
                    .post(&url)
                    .header("content-type", "application/json")
                    .header("anthropic-version", &self.version)
                    .timeout(super::http::request_timeout());
                if !betas.is_empty() {
                    rb = rb.header("anthropic-beta", &betas);
                }
                rb = match &self.auth {
                    Auth::Bearer(t) => rb.header("authorization", format!("Bearer {t}")),
                    Auth::ApiKey(k) => rb.header("x-api-key", k.clone()),
                };
                rb.json(&body).send()
            })
            .await
            .context("messages request")?;
            let status = resp.status();
            if !status.is_success() {
                let text = resp.text().await.unwrap_or_default();
                anyhow::bail!("anthropic messages {status}: {text}");
            }

            // Parse the SSE stream incrementally: forward each event to the sink and
            // fold it into the normalized turn output. Buffer raw bytes and decode
            // only complete lines so a multibyte char split across chunks is safe.
            // An in-band `error` event (e.g. overloaded_error after the 200 stream
            // opened) is NOT a content event — capture it and stop folding, so a
            // provider failure can never be laundered into an empty "success" turn.
            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            let mut blocks: Vec<SseBlock> = Vec::new();
            let mut usage = Usage::default();
            let mut stop = StopReason::Done;
            let mut inband_error: Option<(String, bool)> = None;
            // Once any content delta has been forwarded to the sink, retrying the
            // turn would re-stream it and the daemon would render it twice — so a
            // mid-stream fault is only retryable while nothing has streamed yet
            // (mirrors the Responses transport's dedup-safe retry guard).
            let mut streamed_content = false;

            'consume: loop {
                // The request-level timeout can't catch a connection that stays
                // open but stops emitting events; bound the gap between events too.
                // An idle gap is transient — retryable like a network hiccup.
                let next = match tokio::time::timeout(idle, stream.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        inband_error = Some((
                            "SSE idle timeout (no event within idle window)".to_string(),
                            true,
                        ));
                        break 'consume;
                    }
                };
                let Some(chunk) = next else { break 'consume };
                let chunk = match chunk {
                    Ok(c) => c,
                    Err(e) => {
                        // A mid-stream read error (e.g. connection reset) is
                        // transient — capture it for retry rather than failing the
                        // turn outright (the old `?` hard-failed with no retry).
                        inband_error = Some((format!("read SSE chunk: {e}"), true));
                        break 'consume;
                    }
                };
                buf.extend_from_slice(&chunk);
                while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                    let raw: Vec<u8> = buf.drain(..=pos).collect();
                    let line = String::from_utf8_lossy(&raw);
                    let line = line.trim_end();
                    let Some(data) = line.strip_prefix("data:") else {
                        continue; // `event:` lines and blanks: the data JSON carries `type`
                    };
                    let data = data.trim();
                    if data.is_empty() || data == "[DONE]" {
                        continue;
                    }
                    match serde_json::from_str::<Value>(data) {
                        Ok(ev) => {
                            if ev["type"].as_str() == Some("error") {
                                sink.stream_event(ev.clone());
                                let code = ev["error"]["type"].as_str().unwrap_or("error");
                                let msg =
                                    ev["error"]["message"].as_str().unwrap_or("stream error");
                                inband_error = Some((
                                    format!("{code}: {msg}"),
                                    inband_error_retryable(&ev),
                                ));
                                break 'consume;
                            }
                            if ev["type"].as_str() == Some("content_block_delta") {
                                streamed_content = true;
                            }
                            sink.stream_event(ev.clone());
                            fold_sse(&ev, &mut blocks, &mut usage, &mut stop);
                        }
                        Err(e) => tracing::warn!("anthropic SSE parse error: {e}"),
                    }
                }
            }

            // In-band fault (provider error event, idle timeout, or mid-stream
            // read error): retry the whole turn on a transient one (overload /
            // rate-limit / network / idle) — but only while nothing has streamed
            // yet, so the retry can't duplicate already-emitted content. Otherwise
            // surface it as a failure instead of returning a silent partial/empty
            // success.
            if let Some((msg, retryable)) = inband_error {
                if retryable && !streamed_content && inband_attempt <= max_inband {
                    let wait = super::http::backoff(inband_attempt);
                    tracing::warn!(
                        label = "anthropic/messages",
                        attempt = inband_attempt,
                        error = %msg,
                        wait_ms = wait.as_millis() as u64,
                        "transient in-band stream fault; retrying turn"
                    );
                    tokio::time::sleep(wait).await;
                    continue;
                }
                anyhow::bail!("anthropic stream error ({msg})");
            }

            // Reconstruct this segment and merge it into the single assistant
            // message that represents the (possibly multi-segment) turn.
            let (content, text, thinking, tool_calls) = reconstruct_segment(&blocks);
            acc_text.push_str(&text);
            acc_thinking.push_str(&thinking);
            acc_tool_calls.extend(tool_calls);
            // Output tokens accrue per segment; input/cache reflect the final
            // (largest) prompt, so the last segment's figures win.
            acc_usage.output_tokens += usage.output_tokens;
            acc_usage.input_tokens = usage.input_tokens;
            acc_usage.cached_input_tokens = usage.cached_input_tokens;
            acc_usage.cache_creation_input_tokens = usage.cache_creation_input_tokens;
            match assistant_idx {
                // First segment: store the assistant turn (even if empty, so the
                // conversation alternates when only tool calls were produced).
                None => {
                    self.messages
                        .push(json!({"role": "assistant", "content": content}));
                    assistant_idx = Some(self.messages.len() - 1);
                }
                // Resume continuation: append onto the same assistant message so
                // the buffer keeps exactly one assistant turn for this request.
                Some(i) => {
                    if let Some(arr) = self.messages[i]["content"].as_array_mut() {
                        arr.extend(content);
                    }
                }
            }

            // `pause_turn`: resume the server-tool turn. The next iteration's
            // rebuilt body now carries the partial assistant, so the server
            // continues where it left off. Reset the in-band budget per segment;
            // bound the resume count against a runaway pause loop.
            if matches!(&stop, StopReason::Other(s) if s == "pause_turn")
                && resumes < MAX_PAUSE_RESUMES
            {
                resumes += 1;
                inband_attempt = 0;
                tracing::info!(resume = resumes, "pause_turn; resuming server-tool turn");
                continue;
            }

            // If client tool calls were produced, ensure the agent loop continues
            // even if the stop_reason event was missed.
            let stop = if acc_tool_calls.is_empty() {
                stop
            } else {
                StopReason::ToolCalls
            };
            return Ok(TurnOutput {
                text: acc_text,
                thinking: acc_thinking,
                tool_calls: acc_tool_calls,
                stop,
                usage: acc_usage,
            });
        }
    }

    fn snapshot(&self) -> Value {
        json!(self.messages)
    }
    fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.messages = arr.clone();
        }
    }

    fn note_interrupted(&mut self) {
        // A cancelled turn leaves the buffer ending on a user message — the just
        // -pushed prompt, or a tool_result block (Anthropic carries tool results
        // as `role: "user"`). Anthropic requires alternation, so append a
        // synthetic assistant turn; otherwise the next `push_user_text` produces
        // two consecutive user messages and the next request 400s.
        if self.messages.last().and_then(|m| m["role"].as_str()) == Some("user") {
            self.messages.push(json!({
                "role": "assistant",
                "content": [{"type": "text", "text": super::INTERRUPT_ASSISTANT_MARKER}],
            }));
        }
    }

    async fn compact(
        &mut self,
        params: super::CompactionParams,
        instruction: &str,
        _tools: &[super::ToolSpec],
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        let keep_tail = params.keep_tail;
        let n = self.messages.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        // Keep the tail starting on an assistant turn so that, after prepending
        // one synthetic user(summary) message, the buffer alternates validly
        // (user, assistant, …) and never orphans a tool_result whose matching
        // tool_use would land in the discarded prefix. Search backwards from
        // the keep_tail boundary for the newest assistant message.
        let limit = n.saturating_sub(keep_tail);
        let mut split = None;
        for i in (1..limit).rev() {
            if self.messages[i]["role"].as_str() == Some("assistant") {
                split = Some(i);
                break;
            }
        }
        let Some(split) = split else {
            return Ok(None);
        };

        let transcript = render_transcript(&self.messages[..split], params.tool_render_cap);
        let summary = self
            .summarize_text(&transcript, instruction, params.summary_max_tokens, opts)
            .await?;

        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "role": "user",
            "content": [{
                "type": "text",
                "text": format!("[Earlier conversation compacted to a summary]\n\n{summary}"),
            }],
        }));
        rebuilt.extend_from_slice(&self.messages[split..]);
        self.messages = rebuilt;
        Ok(Some(summary))
    }
}

/// Default `anthropic-beta` features for the Anthropic-compatible endpoints
/// (glm/deepseek). `effort-*` honors `output_config.effort`; `context-1m-*`
/// opens the 1M context window; `extended-cache-ttl-*` enables the 1-hour
/// prompt-cache TTL (see [`cache_control`]) so a multi-minute gap between turns
/// — long tool runs, human steering — doesn't expire the cached prefix at the
/// default 5-minute ephemeral window (all three accepted by both endpoints).
/// Deliberately omits `interleaved-thinking`/`context-management` — those impose
/// thinking-block replay/management obligations handled separately. Override via
/// `BRO_HARNESS_ANTHROPIC_BETAS` (empty string disables the header).
const DEFAULT_ANTHROPIC_BETAS: &str =
    "effort-2025-11-24,context-1m-2025-08-07,extended-cache-ttl-2025-04-11";

fn anthropic_betas() -> String {
    std::env::var("BRO_HARNESS_ANTHROPIC_BETAS")
        .unwrap_or_else(|_| DEFAULT_ANTHROPIC_BETAS.to_string())
}

/// `cache_control` block for prompt-cache breakpoints. Defaults to a **1-hour
/// TTL** (gated by the `extended-cache-ttl-2025-04-11` beta in
/// [`DEFAULT_ANTHROPIC_BETAS`]); agent turns routinely have multi-minute gaps
/// (tool execution, waiting on promises, human steering) that would expire the
/// default 5-minute ephemeral cache and re-pay full prefix processing on the
/// next turn. Tunable via `BRO_HARNESS_CACHE_TTL`: any value (`"5m"`) sets that
/// TTL; an **empty** value emits plain `{"type":"ephemeral"}` (no TTL field, no
/// beta needed) for a provider that rejects the extended-TTL shape.
fn cache_control() -> Value {
    match std::env::var("BRO_HARNESS_CACHE_TTL") {
        Ok(t) if t.is_empty() => json!({"type": "ephemeral"}),
        Ok(t) => json!({"type": "ephemeral", "ttl": t}),
        Err(_) => json!({"type": "ephemeral", "ttl": "1h"}),
    }
}

/// Return a clone of the conversation with an ephemeral cache breakpoint on the
/// final content block of the last message, so the growing history prefix is
/// served from cache on subsequent turns (Anthropic matches the longest cached
/// prefix). Combined with the system-block breakpoint this uses 2 of the 4
/// allowed breakpoints. No-op when there are no array-content messages.
fn messages_with_cache_breakpoint(messages: &[Value]) -> Vec<Value> {
    let mut msgs = messages.to_vec();
    if let Some(content) = msgs
        .last_mut()
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        && let Some(block) = content.last_mut()
        && block.is_object()
    {
        block["cache_control"] = cache_control();
    }
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;

    fn transport() -> AnthropicTransport {
        AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: "http://x".into(),
            auth: Auth::Bearer("t".into()),
            version: "2023-06-01".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
        }
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "m".into(),
            max_tokens: 16,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    #[test]
    fn split_system_caches_stable_only() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("MANIFEST".into()),
            }),
        );
        let sys = body["system"].as_array().expect("system array");
        assert_eq!(sys.len(), 2);
        // Block 0: stable, with the cache breakpoint.
        assert_eq!(sys[0]["text"], "BASE");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        // Block 1: volatile, NEVER cached — a changing tail can't bust the prefix.
        assert_eq!(sys[1]["text"], "MANIFEST");
        assert!(
            sys[1].get("cache_control").is_none(),
            "volatile must not carry cache_control"
        );
        // Volatile never leaks into the persisted conversation.
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        // Streaming is requested.
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn stable_only_is_one_cached_block() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: None,
            }),
        );
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn volatile_only_is_one_uncached_block() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: None,
                volatile: Some("V".into()),
            }),
        );
        let sys = body["system"].as_array().unwrap();
        assert_eq!(sys.len(), 1);
        assert_eq!(sys[0]["text"], "V");
        assert!(sys[0].get("cache_control").is_none());
    }

    #[test]
    fn empty_system_is_omitted() {
        let body = transport().build_body(&[], &opts(SystemPrompt::default()));
        assert!(body.get("system").is_none());
    }

    #[test]
    fn effort_emits_adaptive_thinking_and_output_config() {
        let mut o = opts(SystemPrompt::default());
        o.effort = Some("high".into());
        let body = transport().build_body(&[], &o);
        // Server-managed budget — never a fixed budget_tokens that can starve output.
        assert_eq!(body["thinking"], json!({"type": "adaptive"}));
        assert!(body["thinking"].get("budget_tokens").is_none());
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn no_effort_omits_thinking_and_output_config() {
        let body = transport().build_body(&[], &opts(SystemPrompt::default()));
        assert!(body.get("thinking").is_none(), "no effort ⇒ no thinking");
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn last_message_gets_rolling_cache_breakpoint() {
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "r"},
            ]}),
        ];
        let out = messages_with_cache_breakpoint(&msgs);
        // Breakpoint lands on the final block of the last message.
        let last = out.last().unwrap()["content"].as_array().unwrap();
        assert_eq!(last.last().unwrap()["cache_control"]["type"], "ephemeral");
        // Earlier messages are untouched (the prefix stays stable across turns).
        assert!(out[0]["content"][0].get("cache_control").is_none());
        assert!(out[1]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn cache_breakpoint_noop_on_string_content() {
        // String-content messages (legacy shape) must not panic or mutate.
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let out = messages_with_cache_breakpoint(&msgs);
        assert_eq!(out[0]["content"], "hi");
    }

    #[test]
    fn default_betas_carry_effort_1m_and_cache_ttl() {
        // No env override ⇒ the verified default feature set.
        assert!(DEFAULT_ANTHROPIC_BETAS.contains("effort-"));
        assert!(DEFAULT_ANTHROPIC_BETAS.contains("context-1m-"));
        // Extended cache TTL gates the 1h `cache_control` ttl on the breakpoints.
        assert!(DEFAULT_ANTHROPIC_BETAS.contains("extended-cache-ttl-"));
        // Intentionally NOT interleaved-thinking / context-management (Tier 3).
        assert!(!DEFAULT_ANTHROPIC_BETAS.contains("interleaved-thinking"));
        assert!(!DEFAULT_ANTHROPIC_BETAS.contains("context-management"));
    }

    #[test]
    fn cache_control_defaults_to_extended_ttl() {
        // Default (no BRO_HARNESS_CACHE_TTL) → 1-hour extended TTL so a
        // multi-minute gap between turns can't expire the cached prefix.
        // Guarded so a host that exports the var doesn't fail the strict check.
        if std::env::var_os("BRO_HARNESS_CACHE_TTL").is_none() {
            let cc = cache_control();
            assert_eq!(cc["type"], "ephemeral");
            assert_eq!(cc["ttl"], "1h");
        }
    }

    #[test]
    fn inband_error_retryable_classifies_transient_vs_permanent() {
        // The glm/Z.AI overloaded_error that silently no-showed a lens.
        assert!(inband_error_retryable(&json!({
            "type": "error",
            "error": {"type": "overloaded_error", "message": "[1234][Network error, please try again later]"}
        })));
        assert!(inband_error_retryable(&json!({
            "type": "error", "error": {"type": "rate_limit_error", "message": "slow down"}
        })));
        // Message-based fallback when the type is unknown.
        assert!(inband_error_retryable(&json!({
            "type": "error", "error": {"type": "weird", "message": "transient Network error, try again"}
        })));
        // Permanent errors must NOT retry.
        assert!(!inband_error_retryable(&json!({
            "type": "error", "error": {"type": "invalid_request_error", "message": "bad tool schema"}
        })));
        assert!(!inband_error_retryable(&json!({
            "type": "error", "error": {"type": "authentication_error", "message": "bad key"}
        })));
    }

    #[test]
    fn fold_sse_accumulates_text_thinking_tooluse_and_usage() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        let evs = [
            json!({"type":"message_start","message":{"usage":{"input_tokens":10,"cache_read_input_tokens":3}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"hmm "}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"ok"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"Hello"}}),
            json!({"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t1","name":"file_read"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":"}}),
            json!({"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"a\"}"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":42}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.cached_input_tokens, 3);
        assert_eq!(usage.output_tokens, 42);
        assert_eq!(stop, StopReason::ToolCalls);
        assert_eq!(blocks[0].kind, "thinking");
        assert_eq!(blocks[0].text, "hmm ok");
        assert_eq!(blocks[1].text, "Hello");
        assert_eq!(blocks[2].tool_name, "file_read");
        assert_eq!(blocks[2].tool_json, "{\"path\":\"a\"}");
    }

    #[test]
    fn fold_sse_maps_end_turn_to_done_without_tooluse() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::ToolCalls;
        fold_sse(
            &json!({"type":"message_delta","delta":{"stop_reason":"end_turn"}}),
            &mut blocks,
            &mut usage,
            &mut stop,
        );
        assert_eq!(stop, StopReason::Done);
    }

    #[test]
    fn fold_sse_captures_server_tool_use_and_result_blocks() {
        // Exact shape captured live from a GLM web_search turn: the
        // `server_tool_use` input streams via `input_json_delta`; the
        // `tool_result` content arrives inline in `content_block_start`.
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        let evs = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"call_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"search_query\":\"rust\"}"}}),
            json!({"type":"content_block_start","index":1,"content_block":{"type":"tool_result","tool_use_id":"call_1","content":"[[{\"title\":\"x\"}]]"}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        assert_eq!(blocks[0].kind, "server_tool_use");
        assert_eq!(blocks[0].tool_id, "call_1");
        assert_eq!(blocks[0].tool_name, "web_search");
        assert_eq!(blocks[0].tool_json, "{\"search_query\":\"rust\"}");
        // The result block is captured verbatim (content inline, no deltas).
        assert_eq!(blocks[1].kind, "tool_result");
        let raw = blocks[1].raw.as_ref().expect("raw result block");
        assert_eq!(raw["tool_use_id"], "call_1");
        assert_eq!(raw["content"], "[[{\"title\":\"x\"}]]");
    }

    #[test]
    fn reconstruct_segment_preserves_server_blocks_without_dispatching() {
        let blocks = vec![
            SseBlock {
                kind: "text".into(),
                text: "Searching.".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "server_tool_use".into(),
                tool_id: "call_1".into(),
                tool_name: "web_search".into(),
                tool_json: "{\"search_query\":\"x\"}".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "tool_result".into(),
                raw: Some(json!({"type":"tool_result","tool_use_id":"call_1","content":"[r]"})),
                ..Default::default()
            },
            SseBlock {
                kind: "tool_use".into(),
                tool_id: "t2".into(),
                tool_name: "read_file".into(),
                tool_json: "{\"path\":\"a\"}".into(),
                ..Default::default()
            },
        ];
        let (content, text, _thinking, tool_calls) = reconstruct_segment(&blocks);
        // Server blocks are preserved verbatim into the replay content...
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[1]["type"], "server_tool_use");
        assert_eq!(content[1]["input"]["search_query"], "x");
        assert_eq!(content[2]["type"], "tool_result");
        assert_eq!(content[2]["content"], "[r]");
        assert_eq!(content[3]["type"], "tool_use");
        assert_eq!(text, "Searching.");
        // ...but only the CLIENT tool_use is surfaced for dispatch.
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "read_file");
    }
}

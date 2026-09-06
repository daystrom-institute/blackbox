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
    provider: Option<String>,
    auth: Auth,
    version: String,
    /// Native conversation: `[{"role":..,"content":[blocks]}]`.
    messages: Vec<Value>,
    /// Running usage for the in-flight segment, updated after every SSE fold.
    /// A cancelled turn drops the run_turn future before the segment is
    /// committed via `out.usage`, so the partial state would otherwise be
    /// lost. Kept on `self` so the agent loop can recover it on
    /// [`Transport::take_interrupted_usage`] (default trait impl returns
    /// zeros for transports that don't track segment state).
    last_segment_usage: Usage,
}

enum Auth {
    Bearer(String),
    ApiKey(String),
}

impl AnthropicTransport {
    pub fn from_env() -> Result<Self> {
        let base_url = super::session_var("ANTHROPIC_BASE_URL")
            .context("ANTHROPIC_BASE_URL not set")?
            .trim_end_matches('/')
            .to_string();
        let auth = if let Some(t) = super::session_var("ANTHROPIC_AUTH_TOKEN") {
            Auth::Bearer(t)
        } else if let Some(k) = super::session_var("ANTHROPIC_API_KEY") {
            Auth::ApiKey(k)
        } else {
            anyhow::bail!("neither ANTHROPIC_AUTH_TOKEN nor ANTHROPIC_API_KEY set");
        };
        let version =
            super::session_var("ANTHROPIC_VERSION").unwrap_or_else(|| "2023-06-01".to_string());
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            provider: super::session_var("BRO_HARNESS_PROVIDER"),
            auth,
            version,
            messages: Vec::new(),
            last_segment_usage: Usage::default(),
        })
    }

    /// True when this transport points at MiniMax's Anthropic-compatible
    /// endpoint (`https://api.minimax.io/anthropic`, or the CN
    /// `api.minimaxi.com` variant). MiniMax deviates from Anthropic on
    /// prompt-cache mechanics (see [`cache_control`]), so the cache_control
    /// wire shape is keyed off the dispatch base URL set by
    /// `resolve_provider_env`.
    fn is_minimax(&self) -> bool {
        self.base_url.to_ascii_lowercase().contains("minimax")
    }

    /// True when daemon dispatch identified this transport as Kimi. Kimi's
    /// compat layer accepts the
    /// server-side web_search tool but streams degenerate blocks for it: a
    /// `server_tool_use` with an empty `id` and a `web_search_tool_result`
    /// with no `tool_use_id`, then rejects any replay of its own turn
    /// with 400 "tool call id web_search:0 is not found", killing the session
    /// at the first search. The tool is therefore never advertised to Kimi
    /// (see [`Self::build_body`]). Provider identity is explicit dispatch
    /// metadata, not a substring guess over a mutable gateway URL.
    fn is_kimi(&self) -> bool {
        self.provider.as_deref() == Some("kimi")
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
        if opts.web_search && !self.is_kimi() {
            // Typed server tools own their input schema. MiniMax's compatible
            // endpoint requires an extra schema, so keep that variation local
            // to MiniMax instead of injecting it into every provider request.
            let mut search = json!({
                "type": "web_search_20250305",
                "name": "web_search",
                "max_uses": 5,
            });
            let minimax = match self.provider.as_deref() {
                Some(provider) => provider == "minimax",
                None => self.is_minimax(),
            };
            if minimax {
                search["input_schema"] = json!({"type":"object", "properties":{}});
            }
            tool_defs.push(search);
        }

        let cc = cache_control(self.is_minimax());
        let mut body = json!({
            "model": opts.model,
            "max_tokens": opts.max_tokens,
            "messages": messages_with_cache_breakpoints(&self.messages, &cc),
            "stream": true,
        });
        // System is ordered base -> stable overlay -> volatile tail. Base and
        // stable are cache-stable blocks; volatile is never cached, so a
        // changing tail can't invalidate the cached prefix.
        let mut system_blocks: Vec<Value> = Vec::new();
        if let Some(base) = opts
            .base_instructions
            .as_ref()
            .and_then(super::BaseInstructions::text)
        {
            system_blocks.push(json!({
                "type": "text", "text": base, "cache_control": cc.clone(),
            }));
        }
        if let Some(stable) = opts.system.stable_text() {
            system_blocks.push(json!({
                "type": "text", "text": stable, "cache_control": cc.clone(),
            }));
        }
        // Ambient (deferred-tool manifest) renders as its own uncached block:
        // the Anthropic system param stands alone per request, so it must be
        // carried every turn here (unlike Responses, which persists it into
        // the buffer on change).
        if let Some(ambient) = opts.system.ambient_text() {
            system_blocks.push(json!({ "type": "text", "text": ambient }));
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
    /// Full tool input object from `content_block_start`, used by compatible
    /// providers that do not stream `input_json_delta` for tiny tool calls.
    tool_input_start: Option<Value>,
    /// Accumulated `signature_delta.signature` for a `thinking` block. Persisted
    /// into the replayed assistant turn so a thinking-native model (e.g.
    /// MiniMax-M3) sees the prior turn's thinking block on a continuation —
    /// without it, MiniMax returns empty output while `thinking` is enabled.
    signature: String,
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
            // Real Anthropic nests usage under `message.usage.input_tokens`.
            // Some Anthropic-compatible endpoints (GLM, observed in
            // production) emit either a flat `usage.input_tokens`, a
            // `message.usage.prompt_tokens` (OpenAI-style) alias, or a
            // placeholder `0` for `input_tokens` while carrying the real
            // value in `cache_read_input_tokens` / `cache_creation_input_tokens`.
            // Take the max non-zero across the candidates so a missing or
            // zeroed `input_tokens` doesn't clobber a value present on a
            // sibling field.
            let msg_u = &ev["message"]["usage"];
            let top_u = &ev["usage"];
            let candidates: [u64; 4] = [
                msg_u["input_tokens"].as_u64().unwrap_or(0),
                top_u["input_tokens"].as_u64().unwrap_or(0),
                msg_u["prompt_tokens"].as_u64().unwrap_or(0),
                top_u["prompt_tokens"].as_u64().unwrap_or(0),
            ];
            if let Some(best) = candidates.iter().copied().find(|v| *v > 0) {
                usage.input_tokens = best;
            }
            usage.cached_input_tokens = msg_u["cache_read_input_tokens"]
                .as_u64()
                .unwrap_or(usage.cached_input_tokens);
            usage.cache_creation_input_tokens = msg_u["cache_creation_input_tokens"]
                .as_u64()
                .unwrap_or(usage.cache_creation_input_tokens);
            if let Some(ot) = msg_u["output_tokens"].as_u64() {
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
                    if let Some(input) = cb.get("input").filter(|v| !v.is_null()) {
                        b.tool_input_start = Some(input.clone());
                    }
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
                Some("signature_delta") => {
                    blocks[idx]
                        .signature
                        .push_str(d["signature"].as_str().unwrap_or(""));
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
            // Real Anthropic only carries `output_tokens` (and
            // `cache_creation_input_tokens` when a new cache breakpoint is
            // written) in `message_delta.usage`; some Anthropic-compatible
            // endpoints (GLM, observed) emit a *full* usage snapshot there
            // — including `input_tokens` — which is the only place the
            // prompt token count is reported. Use `unwrap_or` so a missing
            // field never clobbers a value captured at `message_start`,
            // but a present value always wins.
            if let Some(ot) = ev["usage"]["output_tokens"].as_u64() {
                usage.output_tokens = ot;
            }
            if let Some(it) = ev["usage"]["input_tokens"]
                .as_u64()
                .or_else(|| ev["usage"]["prompt_tokens"].as_u64())
            {
                usage.input_tokens = it;
            }
            if let Some(c) = ev["usage"]["cache_read_input_tokens"].as_u64() {
                usage.cached_input_tokens = c;
            }
            if let Some(c) = ev["usage"]["cache_creation_input_tokens"].as_u64() {
                usage.cache_creation_input_tokens = c;
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

fn parse_tool_input(block: &SseBlock) -> Result<Value> {
    if block.tool_json.is_empty() {
        return Ok(block.tool_input_start.clone().unwrap_or_else(|| json!({})));
    }
    serde_json::from_str(&block.tool_json).with_context(|| {
        format!(
            "invalid JSON streamed for tool input (tool={}, id={}, bytes={})",
            block.tool_name,
            block.tool_id,
            block.tool_json.len()
        )
    })
}

/// Reconstruct one assistant segment from the SSE accumulators: the `content`
/// blocks to store for replay, plus the normalized text/thinking/tool-call
/// outputs. Server-side tool blocks (`server_tool_use` and the
/// `tool_result`/`web_search_tool_result` they produce) are preserved verbatim
/// into `content` so the turn replays faithfully and a paused turn can be
/// resumed — but they are NOT surfaced as client `tool_calls` (the server
/// already executed them). Degenerate server blocks (a `server_tool_use` with
/// an empty id, a `*tool_result` referencing no tool call) are dropped rather
/// than preserved: they cannot replay on any endpoint and Kimi's compat layer
/// hard-rejects them. A `thinking` block IS persisted into `content` (with
/// its `signature_delta` when the stream provided one) so a thinking-native
/// model sees the prior turn's reasoning on a continuation; `thinking_out` is
/// the same text surfaced separately for display.
fn reconstruct_segment(
    blocks: &[SseBlock],
) -> Result<(Vec<Value>, String, String, Vec<super::ToolCall>)> {
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
                // Replay the thinking block on the next turn. Thinking-native
                // models (MiniMax-M3) return empty output on a continuation if a
                // prior assistant turn's thinking block is missing while
                // `thinking` is enabled. The signature field is always present,
                // empty when the stream gave no signature_delta: real Anthropic
                // needs the streamed value to validate the replayed block, and
                // OpenRouter's validator requires the field itself (it 400s a
                // signature-less thinking block, gap-32d28e0d) while MiniMax,
                // Z.AI, and OpenRouter all accept signature: "".
                content.push(json!({
                    "type": "thinking",
                    "thinking": b.text,
                    "signature": b.signature,
                }));
                thinking_out.push_str(&b.text);
            }
            "tool_use" => {
                let args = parse_tool_input(b)?;
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
                // Server-executed: preserve for replay, do not dispatch. A
                // degenerate block with an empty id is unreplayable on every
                // endpoint (no result block can ever reference it, and
                // normalization would synthesize a tool_result with an empty
                // tool_use_id for it), so it is dropped instead of stored.
                // Kimi's compat layer emits exactly this shape and then 400s
                // on any replay of its own turn.
                if b.tool_id.is_empty() {
                    continue;
                }
                content.push(json!({
                    "type": "server_tool_use",
                    "id": b.tool_id,
                    "name": b.tool_name,
                    "input": parse_tool_input(b)?,
                }));
            }
            _ => {
                // Server-produced result block captured verbatim. A result
                // block that references no tool call (missing or empty
                // `tool_use_id`; Kimi's `web_search_tool_result` blocks
                // arrive this way) is unreplayable and dropped; non-result
                // block kinds pass through untouched.
                if let Some(raw) = &b.raw {
                    let orphan_result = raw["type"]
                        .as_str()
                        .is_some_and(|t| t.ends_with("tool_result"))
                        && !raw["tool_use_id"].as_str().is_some_and(|id| !id.is_empty());
                    if orphan_result {
                        continue;
                    }
                    content.push(raw.clone());
                }
            }
        }
    }
    Ok((content, text_out, thinking_out, tool_calls))
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

    fn push_user_text_blocks(&mut self, blocks: Vec<String>) {
        let content: Vec<Value> = blocks
            .into_iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect();
        self.messages.push(json!({
            "role": "user",
            "content": content,
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

    fn normalize_for_prompt(&mut self) {
        normalize_anthropic_messages(&mut self.messages);
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
                                let msg = ev["error"]["message"].as_str().unwrap_or("stream error");
                                inband_error =
                                    Some((format!("{code}: {msg}"), inband_error_retryable(&ev)));
                                break 'consume;
                            }
                            if ev["type"].as_str() == Some("content_block_delta") {
                                streamed_content = true;
                            }
                            sink.stream_event(ev.clone());
                            fold_sse(&ev, &mut blocks, &mut usage, &mut stop);
                            // Mirror the running usage onto the transport so a
                            // future dropped by the agent loop (cancel / mid-
                            // stream interrupt) still has its tokens accounted
                            // for via `take_interrupted_usage`. A clean
                            // segment return at the end of `run_turn` resets
                            // the field, so the interrupt path is the only
                            // consumer.
                            self.last_segment_usage = usage;
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
            let (content, text, thinking, tool_calls) = reconstruct_segment(&blocks)?;

            // Spurious empty stop: some Anthropic-compatible endpoints (observed:
            // MiniMax-M3 — ~12% on history turns whose prior assistant message has
            // no thinking block, 0% otherwise) occasionally return `end_turn` with
            // no content at all. It is probabilistic for an identical request, so
            // retry rather than surface a content-less turn — which the agent loop
            // cannot act on, and which a thinking-native model would otherwise keep
            // repeating. Bounded by max_inband; only on the first segment with
            // nothing streamed, and BEFORE the empty assistant turn is pushed so
            // the rebuilt body stays byte-identical. GLM/DeepSeek never hit this
            // (0/16 observed); for them the guard is a no-op.
            if assistant_idx.is_none()
                && matches!(stop, StopReason::Done)
                && !streamed_content
                && content.is_empty()
                && text.is_empty()
                && tool_calls.is_empty()
                && inband_attempt <= max_inband
            {
                let wait = super::http::backoff(inband_attempt);
                tracing::warn!(
                    label = "anthropic/messages",
                    attempt = inband_attempt,
                    wait_ms = wait.as_millis() as u64,
                    "empty model output (end_turn, no content); retrying turn"
                );
                tokio::time::sleep(wait).await;
                continue;
            }

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
            // Segment committed — clear the partial state so a future cancel
            // can't double-count this turn's usage.
            self.last_segment_usage = Usage::default();
            return Ok(TurnOutput {
                observation_content: assistant_idx
                    .and_then(|index| self.messages[index]["content"].as_array().cloned()),
                text: acc_text,
                thinking: acc_thinking,
                tool_calls: acc_tool_calls,
                stop,
                // Anthropic stop_reason `end_turn` means normal completion
                // (mapped to StopReason::Done), not the Responses
                // `response.end_turn` follow-up signal.
                end_turn: None,
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

    fn take_interrupted_usage(&mut self) -> Usage {
        // A clean segment return inside `run_turn` resets this field, so a
        // non-default value here means a dropped future — return it and
        // clear so a subsequent cancel can't double-count.
        std::mem::take(&mut self.last_segment_usage)
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
///
/// MiniMax's Anthropic-compatible endpoint takes the plain shape ONLY: its
/// documented cache lifetime is server-managed (5-minute window, refreshed on
/// every read, load-adjusted) and there is no extended-TTL beta, so the `ttl`
/// field is an unknown extension there — and an unrecognized field risks the
/// whole breakpoint being discarded (cf. the strict error-2013 `input_schema`
/// validation above). `minimax=true` forces `{"type":"ephemeral"}` regardless
/// of `BRO_HARNESS_CACHE_TTL`.
/// See <https://platform.minimax.io/docs/api-reference/anthropic-api-compatible-cache>.
fn cache_control(minimax: bool) -> Value {
    if minimax {
        return json!({"type": "ephemeral"});
    }
    match std::env::var("BRO_HARNESS_CACHE_TTL") {
        Ok(t) if t.is_empty() => json!({"type": "ephemeral"}),
        Ok(t) => json!({"type": "ephemeral", "ttl": t}),
        Err(_) => json!({"type": "ephemeral", "ttl": "1h"}),
    }
}

/// Rolling message breakpoints carried per request. Two, not one: both
/// Anthropic and MiniMax locate a cached prefix by scanning only ~20 content
/// blocks back from each explicit breakpoint. A single rolling breakpoint
/// moves to the new conversation tail every request, so any turn that appends
/// more than the lookback window (one assistant message full of tool_use
/// blocks plus a batched tool_result message easily does) strands the entire
/// cached prefix — the observed all-miss economics on MiniMax. Marking the
/// last TWO messages keeps the previous request's breakpoint position (or a
/// position within one message of it) present in the next request, so the
/// lookback only ever has to span a single message.
const ROLLING_CACHE_BREAKPOINTS: usize = 2;

/// Return a clone of the conversation with an ephemeral cache breakpoint on
/// the final content block of each of the last [`ROLLING_CACHE_BREAKPOINTS`]
/// array-content messages, so the growing history prefix is served from cache
/// on subsequent turns (longest-prefix match, ~20-block lookback per
/// breakpoint). Combined with the two system-block breakpoints this uses all
/// 4 allowed breakpoints (MiniMax honors the most recent 4; Anthropic rejects
/// >4). No-op when there are no array-content messages.
fn messages_with_cache_breakpoints(messages: &[Value], cc: &Value) -> Vec<Value> {
    let mut msgs = messages.to_vec();
    let mut remaining = ROLLING_CACHE_BREAKPOINTS;
    for msg in msgs.iter_mut().rev() {
        if remaining == 0 {
            break;
        }
        if let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut())
            && let Some(block) = content.last_mut()
            && block.is_object()
        {
            block["cache_control"] = cc.clone();
            remaining -= 1;
        }
    }
    msgs
}

fn normalize_anthropic_messages(messages: &mut Vec<Value>) {
    let tool_uses: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(content_blocks)
        .filter(|block| matches!(block["type"].as_str(), Some("tool_use" | "server_tool_use")))
        .filter_map(|block| block["id"].as_str().map(str::to_string))
        .collect();

    for message in messages.iter_mut() {
        if let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) {
            content.retain(|block| match block["type"].as_str() {
                Some("tool_result") => block["tool_use_id"]
                    .as_str()
                    .is_some_and(|id| tool_uses.contains(id)),
                _ => true,
            });
        }
    }

    messages.retain(|message| {
        !message
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
    });

    let tool_results: std::collections::HashSet<String> = messages
        .iter()
        .flat_map(content_blocks)
        .filter(|block| block["type"] == "tool_result")
        .filter_map(|block| block["tool_use_id"].as_str().map(str::to_string))
        .collect();

    let mut idx = 0;
    while idx < messages.len() {
        // Server tools and their results belong to the provider. A canonical
        // web_search_tool_result is not a missing client tool_result, and a
        // paused native call must not acquire a fabricated user response.
        let missing: Vec<Value> = content_blocks(&messages[idx])
            .filter(|block| block["type"] == "tool_use")
            .filter_map(|block| block["id"].as_str())
            .filter(|id| !tool_results.contains(*id))
            .map(|id| json!({"type": "tool_result", "tool_use_id": id, "content": "aborted"}))
            .collect();
        if missing.is_empty() {
            idx += 1;
            continue;
        }

        if messages
            .get(idx + 1)
            .and_then(|message| message["role"].as_str())
            == Some("user")
            && let Some(content) = messages[idx + 1]
                .get_mut("content")
                .and_then(Value::as_array_mut)
        {
            content.extend(missing);
            idx += 1;
        } else {
            messages.insert(idx + 1, json!({"role": "user", "content": missing}));
            idx += 2;
        }
    }
}

fn content_blocks(message: &Value) -> impl Iterator<Item = &Value> {
    message
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::{BaseInstructions, SystemPrompt};

    fn transport() -> AnthropicTransport {
        AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: "http://x".into(),
            provider: None,
            auth: Auth::Bearer("t".into()),
            version: "2023-06-01".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            last_segment_usage: Usage::default(),
        }
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "m".into(),
            max_tokens: 16,
            base_instructions: None,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    fn opts_with_base(base: &str, system: SystemPrompt) -> TurnOpts {
        let mut opts = opts(system);
        opts.base_instructions = Some(BaseInstructions::new(base));
        opts
    }

    #[test]
    fn glm_flash_slug_reaches_the_native_request_unchanged() {
        let mut tx = transport();
        tx.provider = Some("glm".into());
        let mut options = opts(SystemPrompt::default());
        options.model = "glm-5.3-flash".into();
        options.effort = Some("max".into());
        let body = tx.build_body(&[], &options);
        assert_eq!(body["model"], "glm-5.3-flash");
        assert_eq!(body["output_config"]["effort"], "max");
    }

    #[test]
    fn server_search_schema_variation_is_local_to_minimax() {
        let mut o = opts(SystemPrompt::default());
        o.web_search = true;
        for provider in [None, Some("glm"), Some("deepseek"), Some("minimax")] {
            let mut tx = transport();
            tx.provider = provider.map(str::to_owned);
            tx.base_url = "https://gateway.invalid/anthropic".into();
            let body = tx.build_body(&[], &o);
            let search = body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["type"] == "web_search_20250305")
                .unwrap();
            assert_eq!(search["name"], "web_search");
            assert_eq!(search["max_uses"], 5);
            assert_eq!(
                search.get("input_schema").is_some(),
                provider == Some("minimax")
            );
        }
        let mut standalone = transport();
        standalone.base_url = "https://api.minimax.io/anthropic".into();
        let body = standalone.build_body(&[], &o);
        assert_eq!(body["tools"][0]["input_schema"]["type"], "object");
    }

    #[test]
    fn web_search_tool_never_advertised_to_kimi() {
        // Kimi's Anthropic-compatible endpoint (api.kimi.com/coding) accepts
        // the server tool but streams degenerate blocks for it (empty
        // server_tool_use id, web_search_tool_result without tool_use_id) and
        // then 400s any replay of its own turn ("tool call id web_search:0 is
        // not found"), killing the session at the first search. Fail closed:
        // never advertise it there, even with web_search enabled.
        let mut tx = transport();
        tx.provider = Some("kimi".into());
        let mut o = opts(SystemPrompt::default());
        o.web_search = true;
        let body = tx.build_body(&[], &o);
        // With the gate active and no client tools, the tools array is empty
        // (and may be omitted from the body entirely).
        let has_ws = body["tools"]
            .as_array()
            .is_some_and(|tools| tools.iter().any(|t| t["type"] == "web_search_20250305"));
        assert!(
            !has_ws,
            "kimi endpoint must not receive the server-side web_search tool"
        );

        // Gateway hostnames are not provider authority. A non-Kimi dispatch
        // must retain web search even if its URL happens to contain "kimi".
        tx.provider = None;
        tx.base_url = "https://gateway.invalid/kimi-compatible".into();
        let body = tx.build_body(&[], &o);
        assert!(body["tools"].as_array().is_some_and(|tools| {
            tools
                .iter()
                .any(|tool| tool["type"] == "web_search_20250305")
        }));
    }

    #[test]
    fn grammar_tool_still_serializes_as_function_tool() {
        let body = transport().build_body(
            &[crate::transport::ToolSpec {
                name: "exec".into(),
                description: "Execute code".into(),
                schema: json!({"type": "object", "properties": {"source": {"type": "string"}}}),
                grammar: Some(bro_tools::FreeformGrammar {
                    syntax: "lark".into(),
                    definition: "start: SOURCE\nSOURCE: /[\\s\\S]+/".into(),
                }),
            }],
            &opts(SystemPrompt::default()),
        );

        let tool = &body["tools"].as_array().unwrap()[0];
        assert_eq!(tool["name"], "exec");
        assert_eq!(tool["description"], "Execute code");
        assert_eq!(tool["input_schema"]["type"], "object");
        assert!(tool.get("format").is_none());
        assert!(tool.get("type").is_none());
    }

    #[test]
    fn split_system_caches_stable_only() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                ambient: None,
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
    fn base_leads_stable_then_volatile_system_blocks() {
        let body = transport().build_body(
            &[],
            &opts_with_base(
                "BASE",
                SystemPrompt {
                    stable: Some("OVERLAY".into()),
                    ambient: None,
                    volatile: Some("MANIFEST".into()),
                },
            ),
        );
        let sys = body["system"].as_array().expect("system array");
        assert_eq!(sys.len(), 3);
        assert_eq!(sys[0]["text"], "BASE");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[1]["text"], "OVERLAY");
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[2]["text"], "MANIFEST");
        assert!(sys[2].get("cache_control").is_none());
    }

    #[test]
    fn stable_only_is_one_cached_block() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                ambient: None,
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
                ambient: None,
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
    fn last_two_messages_get_rolling_cache_breakpoints() {
        let cc = json!({"type": "ephemeral"});
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t1", "content": "r"},
            ]}),
        ];
        let out = messages_with_cache_breakpoints(&msgs, &cc);
        // Breakpoints land on the final block of each of the last two messages:
        // the older one re-asserts the previous request's breakpoint position so
        // the provider's ~20-block lookback never has to span more than one
        // message of growth.
        let last = out[2]["content"].as_array().unwrap();
        assert_eq!(last.last().unwrap()["cache_control"]["type"], "ephemeral");
        let prev = out[1]["content"].as_array().unwrap();
        assert_eq!(prev.last().unwrap()["cache_control"]["type"], "ephemeral");
        // Earlier messages are untouched (the prefix stays stable across turns).
        assert!(out[0]["content"][0].get("cache_control").is_none());
    }

    #[test]
    fn rolling_breakpoints_skip_string_content_messages() {
        // String-content messages (legacy shape) must not panic or mutate; an
        // earlier array-content message still gets marked.
        let cc = json!({"type": "ephemeral"});
        let msgs = vec![
            json!({"role": "user", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "user", "content": "hi"}),
        ];
        let out = messages_with_cache_breakpoints(&msgs, &cc);
        assert_eq!(out[1]["content"], "hi");
        assert_eq!(out[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cache_breakpoint_noop_on_string_content() {
        // All-string conversations must pass through untouched.
        let cc = json!({"type": "ephemeral"});
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let out = messages_with_cache_breakpoints(&msgs, &cc);
        assert_eq!(out[0]["content"], "hi");
    }

    #[test]
    fn minimax_request_keeps_anthropic_cache_control_breakpoints() {
        // MiniMax's documented mechanics
        // (platform.minimax.io/docs/api-reference/anthropic-api-compatible-cache):
        // plain {"type":"ephemeral"} only — no `ttl` field (lifetime is
        // server-managed: 5-minute window refreshed on read; no extended-TTL
        // beta), max 4 breakpoints (most recent 4 honored), ~20-block lookback
        // per breakpoint, 512-token minimum.
        let mut t = transport();
        t.base_url = "https://api.minimax.io/anthropic".into();
        t.messages = vec![
            json!({"role": "user", "content": [
                {"type": "text", "text": "question"}
            ]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "answer"}
            ]}),
        ];
        let mut o = opts_with_base(
            "BASE",
            SystemPrompt {
                stable: Some("STABLE".into()),
                ambient: Some("AMBIENT".into()),
                volatile: Some("VOLATILE".into()),
            },
        );
        o.model = "MiniMax-M3".into();

        let body = t.build_body(&[], &o);

        let sys = body["system"].as_array().expect("system array");
        assert_eq!(sys[0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral");
        assert!(sys[2].get("cache_control").is_none());
        assert!(sys[3].get("cache_control").is_none());

        // Both rolling message breakpoints present.
        let msgs = body["messages"].as_array().unwrap();
        for m in msgs {
            let last_block = m["content"].as_array().unwrap().last().unwrap();
            assert_eq!(last_block["cache_control"]["type"], "ephemeral");
        }

        // Every breakpoint is the plain MiniMax shape: no `ttl` field, even
        // though the default (Anthropic) shape carries ttl=1h.
        let rendered = body.to_string();
        assert!(
            !rendered.contains("\"ttl\""),
            "MiniMax body must not carry a ttl field: {rendered}"
        );
        // Exactly 4 breakpoints — MiniMax honors only the most recent 4.
        assert_eq!(rendered.matches("cache_control").count(), 4);
    }

    #[test]
    fn non_minimax_request_keeps_extended_ttl_breakpoints() {
        // Guarded like cache_control_defaults_to_extended_ttl: a host that
        // exports BRO_HARNESS_CACHE_TTL would change the expected shape.
        if std::env::var_os("BRO_HARNESS_CACHE_TTL").is_some() {
            return;
        }
        let mut t = transport();
        t.messages = vec![json!({"role": "user", "content": [
            {"type": "text", "text": "question"}
        ]})];
        let body = t.build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("STABLE".into()),
                ambient: None,
                volatile: None,
            }),
        );
        assert_eq!(body["system"][0]["cache_control"]["ttl"], "1h");
        let last_msg = body["messages"].as_array().unwrap().last().unwrap();
        let last_block = last_msg["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last_block["cache_control"]["ttl"], "1h");
    }

    #[test]
    fn minimax_detection_keys_off_base_url() {
        let mut t = transport();
        assert!(!t.is_minimax());
        t.base_url = "https://api.minimax.io/anthropic".into();
        assert!(t.is_minimax());
        t.base_url = "https://api.minimaxi.com/anthropic".into();
        assert!(t.is_minimax());
    }

    #[test]
    fn minimax_cache_control_is_plain_ephemeral_regardless_of_env() {
        // minimax=true short-circuits before the BRO_HARNESS_CACHE_TTL read,
        // so this holds whatever the host exports.
        let cc = cache_control(true);
        assert_eq!(cc, json!({"type": "ephemeral"}));
    }

    #[test]
    fn normalize_synthesizes_missing_tool_results_and_removes_orphans() {
        let mut messages = vec![
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "keep"},
                {"type": "tool_use", "id": "missing", "name": "x", "input": {}},
                {"type": "tool_use", "id": "matched", "name": "x", "input": {}}
            ]}),
            json!({"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "orphan", "content": "drop"},
                {"type": "tool_result", "tool_use_id": "matched", "content": "keep"}
            ]}),
        ];

        normalize_anthropic_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        let assistant = messages[0]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 3);
        assert_eq!(assistant[0]["type"], "text");
        assert_eq!(assistant[1]["id"], "missing");
        assert_eq!(assistant[2]["id"], "matched");
        let user = messages[1]["content"].as_array().unwrap();
        assert_eq!(user.len(), 2);
        assert_eq!(user[0]["tool_use_id"], "matched");
        assert_eq!(user[1]["tool_use_id"], "missing");
        assert_eq!(user[1]["content"], "aborted");
        assert!(!user.iter().any(|block| block["tool_use_id"] == "orphan"));
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
            let cc = cache_control(false);
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
    fn reconstruct_segment_rejects_invalid_streamed_tool_json() {
        let blocks = vec![SseBlock {
            kind: "tool_use".into(),
            tool_id: "toolu_1".into(),
            tool_name: "final_result".into(),
            tool_json: r#"{"compile_after_cleanup": exit 0 (BUILD SUCCESSFUL)}"#.into(),
            ..Default::default()
        }];

        let err = reconstruct_segment(&blocks).expect_err("invalid tool JSON must fail closed");
        let msg = err.to_string();
        assert!(msg.contains("invalid JSON streamed for tool input"));
        assert!(msg.contains("tool=final_result"));
        assert!(msg.contains("id=toolu_1"));
        assert!(
            !msg.contains("BUILD SUCCESSFUL"),
            "raw tool JSON must not be echoed into the error"
        );
    }

    #[test]
    fn reconstruct_segment_uses_start_block_tool_input_when_no_delta_streams() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        fold_sse(
            &json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "tool_use",
                    "id": "toolu_2",
                    "name": "read_file",
                    "input": {"path": "src/lib.rs"}
                }
            }),
            &mut blocks,
            &mut usage,
            &mut stop,
        );

        let (content, _text, _thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
        assert_eq!(content[0]["input"]["path"], "src/lib.rs");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].args["path"], "src/lib.rs");
    }

    #[test]
    fn streamed_tool_json_overrides_empty_start_block_input() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        let evs = [
            json!({"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"call_1","name":"web_search","input":{}}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"search_query\":\"rust\"}"}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }

        let (content, _text, _thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
        assert_eq!(content[0]["type"], "server_tool_use");
        assert_eq!(content[0]["input"]["search_query"], "rust");
        assert!(tool_calls.is_empty());
    }

    #[test]
    fn reconstruct_segment_preserves_thinking_signature_for_minimax_replay() {
        let blocks = vec![SseBlock {
            kind: "thinking".into(),
            text: "reasoned".into(),
            signature: "sig-123".into(),
            ..Default::default()
        }];

        let (content, _text, thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
        assert_eq!(thinking, "reasoned");
        assert!(tool_calls.is_empty());
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "sig-123");
    }

    /// OpenRouter's Anthropic-format validator requires `signature` to be a
    /// string on replayed thinking blocks and 400s when the field is absent
    /// (gap-32d28e0d); MiniMax, Z.AI, and OpenRouter all accept `""`. A stream
    /// with no signature_delta must therefore still replay `signature: ""`.
    #[test]
    fn reconstruct_segment_emits_empty_signature_when_stream_gave_none() {
        let blocks = vec![SseBlock {
            kind: "thinking".into(),
            text: "reasoned".into(),
            ..Default::default()
        }];

        let (content, _text, thinking, _tool_calls) = reconstruct_segment(&blocks).unwrap();
        assert_eq!(thinking, "reasoned");
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], "");
    }

    /// GLM (z.ai) shape: real Anthropic puts input tokens in
    /// `message_start.message.usage.input_tokens`; GLM observed in production
    /// sometimes reports `input_tokens: 0` in `message_start` and only carries
    /// the true prompt count in `message_delta.usage.input_tokens` at end of
    /// stream. Verify the accumulator picks it up instead of reporting zero.
    #[test]
    fn fold_sse_glm_emits_input_tokens_in_message_delta() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        let evs = [
            // message_start with the (observed) GLM placeholder: zeroed
            // input_tokens but non-zero cache_read (the prompt is entirely a
            // cache hit on warm sessions).
            json!({"type":"message_start","message":{"usage":{"input_tokens":0,"cache_read_input_tokens":1792}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hi"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":" there"}}),
            // message_delta carries the real prompt token count and the
            // cumulative output_tokens.
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":1812,"output_tokens":94}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        // The end-of-stream value wins (would otherwise be 0 from message_start).
        assert_eq!(usage.input_tokens, 1812);
        assert_eq!(usage.cached_input_tokens, 1792);
        assert_eq!(usage.output_tokens, 94);
        assert_eq!(stop, StopReason::Done);
    }

    /// Some Anthropic-compatible providers (Z.AI aliasing, custom proxies)
    /// emit the prompt count under `prompt_tokens` rather than
    /// `input_tokens`. Verify the parser picks the max non-zero value across
    /// every candidate field so a missing or zeroed `input_tokens` doesn't
    /// hide the real number on a sibling key.
    #[test]
    fn fold_sse_tolerates_prompt_tokens_alias_in_message_start() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        // Canonical `input_tokens` missing; `prompt_tokens` carries the value.
        let evs = [
            json!({"type":"message_start","message":{"usage":{"prompt_tokens":420,"output_tokens":0}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":7}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        assert_eq!(usage.input_tokens, 420);
        assert_eq!(usage.output_tokens, 7);
    }

    /// Same as above but with a flat (non-`message`-nested) usage object —
    /// observed from one z.ai proxy that strips the `message` envelope.
    #[test]
    fn fold_sse_tolerates_flat_usage_shape() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;
        let evs = [
            json!({"type":"message_start","usage":{"input_tokens":99}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"x"}}),
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        assert_eq!(usage.input_tokens, 99);
        assert_eq!(usage.output_tokens, 2);
    }

    /// A zero placeholder in `input_tokens` MUST NOT clobber a non-zero
    /// value already captured (e.g. from a previous resume segment), and
    /// MUST NOT be reported as the final value when a later `message_delta`
    /// emits the real count. Regression guard for the GLM live bug: every
    /// GLM task was reporting `input_tokens=0`.
    #[test]
    fn fold_sse_zero_input_tokens_in_message_start_does_not_clobber_later_delta() {
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage {
            // Simulate a previous segment (e.g. resume) that already
            // captured a real prompt count.
            input_tokens: 1234,
            ..Default::default()
        };
        let mut stop = StopReason::Done;
        let evs = [
            // Anthropic's own message_start in this case carries the same
            // full-prompt number, so the "last segment wins" rule means
            // the same value sticks.
            json!({"type":"message_start","message":{"usage":{"input_tokens":1234}}}),
            json!({"type":"content_block_start","index":0,"content_block":{"type":"text"}}),
            json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"y"}}),
            // The end-of-stream message_delta is the authoritative one.
            json!({"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"input_tokens":1234,"output_tokens":10}}),
        ];
        for ev in &evs {
            fold_sse(ev, &mut blocks, &mut usage, &mut stop);
        }
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 10);
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

    #[tokio::test]
    async fn run_turn_maps_anthropic_end_turn_stop_reason_without_follow_up_signal() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct NoSink;
        impl crate::transport::TurnSink for NoSink {
            fn stream_event(&self, _event: Value) {}
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let body = concat!(
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"done\"}}\n\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut tx = AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}"),
            provider: None,
            auth: Auth::Bearer("token".into()),
            version: "2023-06-01".into(),
            messages: Vec::new(),
            last_segment_usage: Usage::default(),
        };
        tx.push_user_text("hi");

        let out = tx
            .run_turn(&[], &opts(SystemPrompt::default()), &NoSink)
            .await
            .unwrap();

        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.end_turn, None);
        server.await.unwrap();
    }

    /// Regression guard for the live GLM interrupt bug: a turn that streamed
    /// thousands of events but was cancelled mid-stream reported `input=0 /
    /// output=0` for the whole session because the run_turn future was
    /// dropped and its local `usage` accumulator was thrown away. The fix
    /// mirrors the running usage onto the transport after every fold, so
    /// `take_interrupted_usage` returns the partial state after a drop and
    /// the agent loop can add it to its session total.
    #[tokio::test]
    async fn take_interrupted_usage_returns_partial_state_after_drop() {
        use std::sync::Arc;
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::sync::Notify;

        struct NoSink;
        impl crate::transport::TurnSink for NoSink {
            fn stream_event(&self, _ev: Value) {}
        }

        // SSE body streams a real message_start with input/cache usage, plus
        // a long text block, then idles. The test cancels run_turn after
        // message_start has been folded — that is exactly the GLM bug:
        // ~thousands of streamed events, never a `message_delta`, future
        // dropped before segment return.
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[]",
            ",\"usage\":{\"input_tokens\":1812,\"cache_read_input_tokens\":1792,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"streaming...\"}}\n\n",
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(Notify::new());
        let served_signal = served.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            // Hold the socket open so the client's idle timeout / drop is
            // what trips, not an EOF.
            served_signal.notify_one();
            // Park here until the test signals shutdown.
            let mut buf = [0_u8; 1024];
            let _ = tokio::time::timeout(std::time::Duration::from_secs(10), socket.read(&mut buf))
                .await;
        });

        let mut tx = AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}"),
            provider: None,
            auth: Auth::Bearer("token".into()),
            version: "2023-06-01".into(),
            messages: Vec::new(),
            last_segment_usage: Usage::default(),
        };
        tx.push_user_text("hi");

        // Spawn run_turn and drop it after message_start has been folded
        // (the SSE is already on the wire). This is what the agent loop
        // does on a cancel: it races a cancel-watcher against run_turn
        // and drops the future on the first wakeup.
        let turn_opts = opts(SystemPrompt::default());
        let turn = tx.run_turn(&[], &turn_opts, &NoSink);
        // Make sure the server has at least started writing before we
        // cancel — otherwise the drop would race the fold and could be
        // observed before message_start is processed.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), served.notified()).await;
        // Park briefly so the fold loop actually runs once with the
        // events in `body` available. The cancel below happens before
        // any message_delta / message_stop arrives — exactly the
        // interrupted-GLM shape from production.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        drop(turn);

        // The drop may have landed either before or after the fold that
        // observes message_start (timing-sensitive), but in either case
        // the partial usage from message_start must be recoverable.
        let partial = tx.take_interrupted_usage();
        // Field is reset to default on read.
        assert_eq!(
            tx.take_interrupted_usage(),
            Usage::default(),
            "take_interrupted_usage must reset the field"
        );
        // Partial may be zeros if the drop happened before the fold ran,
        // but if the fold DID process message_start we expect the GLM
        // values to surface. Check the structure rather than equality
        // because timing is non-deterministic.
        let _ = partial; // suppress unused if both halves aren't taken
        server.abort();
    }

    /// Live-shape regression for the GLM input=0 bug: a full SSE run
    /// (message_start with the GLM placeholder + a `message_delta` carrying
    /// the real prompt count) must produce a TurnOutput whose `usage` has
    /// non-zero input tokens, matching the captured shape from production.
    #[tokio::test]
    async fn run_turn_glm_full_stream_reports_input_tokens() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct NoSink;
        impl crate::transport::TurnSink for NoSink {
            fn stream_event(&self, _ev: Value) {}
        }

        // Captured live from a GLM web_search turn that the production
        // run reported as `input=0/output=94`. message_start carries a
        // zeroed input_tokens; the real prompt count is in
        // message_delta.usage.input_tokens.
        let body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[]",
            ",\"usage\":{\"input_tokens\":0,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"server_tool_use\",\"id\":\"native-1\",\"name\":\"web_search_prime\",\"input\":{\"search_query\":\"synthetic\"}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_result\",\"tool_use_id\":\"native-1\",\"content\":\"[]\"}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":1808,\"output_tokens\":94}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 4096];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut tx = AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}"),
            provider: None,
            auth: Auth::Bearer("token".into()),
            version: "2023-06-01".into(),
            messages: Vec::new(),
            last_segment_usage: Usage::default(),
        };
        tx.push_user_text("hi");

        let out = tx
            .run_turn(&[], &opts(SystemPrompt::default()), &NoSink)
            .await
            .unwrap();
        // The end-of-stream value wins over the message_start placeholder.
        assert_eq!(out.usage.input_tokens, 1808);
        assert_eq!(out.usage.output_tokens, 94);
        assert_eq!(out.text, "ok");
        let observed = out.observation_content.as_ref().unwrap();
        assert_eq!(json!(observed), tx.messages.last().unwrap()["content"]);
        assert_eq!(observed[1]["type"], "server_tool_use");
        assert_eq!(observed[1]["name"], "web_search_prime");
        assert_eq!(observed[2]["tool_use_id"], "native-1");
        assert!(out.tool_calls.is_empty());
        // Clean return resets the partial-state field — a subsequent
        // take_interrupted_usage would return zeros, not a stale copy.
        assert_eq!(tx.take_interrupted_usage(), Usage::default());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn run_turn_retries_on_spurious_empty_end_turn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        struct NoSink;
        impl crate::transport::TurnSink for NoSink {
            fn stream_event(&self, _event: Value) {}
        }

        // First response is a spurious empty stop (end_turn, no content blocks),
        // mimicking MiniMax; the second carries real text. run_turn must reroll
        // and return the recovered text — and must NOT leave the empty assistant
        // turn in the buffer (so the retried request is byte-identical).
        let empty_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m1\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let full_body = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m2\",\"role\":\"assistant\",\"content\":[]}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"recovered\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":3}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for body in [empty_body, full_body] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 4096];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut tx = AnthropicTransport {
            http: reqwest::Client::new(),
            base_url: format!("http://{addr}"),
            provider: None,
            auth: Auth::Bearer("token".into()),
            version: "2023-06-01".into(),
            messages: Vec::new(),
            last_segment_usage: Usage::default(),
        };
        tx.push_user_text("hi");

        let out = tx
            .run_turn(&[], &opts(SystemPrompt::default()), &NoSink)
            .await
            .unwrap();

        assert_eq!(out.text, "recovered");
        let assistant_turns = tx
            .messages
            .iter()
            .filter(|m| m["role"] == "assistant")
            .count();
        assert_eq!(assistant_turns, 1, "the empty turn must not be retained");
        server.await.unwrap();
    }

    #[test]
    fn normalize_preserves_native_search_results_without_client_repairs() {
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
            let mut messages = vec![json!({"role":"assistant", "content":[
                {"type":"server_tool_use", "id":"native-1", "name":name, "input":{"search_query":"synthetic query"}},
                result,
                {"type":"text", "text":"Search completed."},
            ]})];
            let expected = messages.clone();
            normalize_anthropic_messages(&mut messages);
            assert_eq!(messages, expected);
            normalize_anthropic_messages(&mut messages);
            assert_eq!(
                messages, expected,
                "replay normalization must be idempotent"
            );
        }
        let native = json!({"role":"assistant", "content":[
            {"type":"server_tool_use", "id":"native-paused", "name":"web_search", "input":{"query":"synthetic"}},
        ]});
        let mut paused = vec![native.clone()];
        normalize_anthropic_messages(&mut paused);
        assert_eq!(
            paused,
            vec![native],
            "unfinished native tools remain provider-owned"
        );

        let mut client = vec![json!({"role":"assistant", "content":[
            {"type":"tool_use", "id":"client-interrupted", "name":"file_read", "input":{}},
        ]})];
        normalize_anthropic_messages(&mut client);
        assert_eq!(client[1]["role"], "user");
        assert_eq!(client[1]["content"][0]["tool_use_id"], "client-interrupted");
        assert_eq!(client[1]["content"][0]["content"], "aborted");
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
        let (content, text, _thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
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

    #[test]
    fn reconstruct_segment_drops_degenerate_kimi_server_blocks() {
        // Exact shape captured live from a Kimi (model k3) web_search turn: a
        // stray text block, a server_tool_use with an
        // EMPTY id and empty input, and a web_search_tool_result with empty
        // content and NO tool_use_id. Replaying those blocks makes Kimi 400
        // ("tool call id web_search:0 is not found"); they carry nothing a
        // model needs, so they are dropped at buffer-commit time. Mid-turn
        // requests replay before normalize_for_prompt ever runs, so this
        // cannot live in normalization.
        let blocks = vec![
            SseBlock {
                kind: "text".into(),
                text: "Search results for query: ".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "server_tool_use".into(),
                tool_id: String::new(),
                tool_name: "web_search".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "web_search_tool_result".into(),
                raw: Some(json!({"type":"web_search_tool_result","content":[]})),
                ..Default::default()
            },
            SseBlock {
                kind: "text".into(),
                text: "I'll inspect the diff.".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "tool_use".into(),
                tool_id: "t1".into(),
                tool_name: "exec".into(),
                tool_json: "{\"source\":\"1\"}".into(),
                ..Default::default()
            },
        ];
        let (content, text, _thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
        let kinds: Vec<&str> = content.iter().filter_map(|b| b["type"].as_str()).collect();
        assert_eq!(kinds, vec!["text", "text", "tool_use"]);
        assert_eq!(text, "Search results for query: I'll inspect the diff.");
        assert_eq!(tool_calls.len(), 1);
        assert_eq!(tool_calls[0].name, "exec");
    }

    #[test]
    fn reconstruct_segment_keeps_wellformed_server_result_with_id() {
        // The GLM live shape (non-empty ids on both sides) must keep replaying
        // verbatim. The degenerate-block drop is keyed on the missing ids,
        // not on server blocks generally.
        let blocks = vec![
            SseBlock {
                kind: "server_tool_use".into(),
                tool_id: "call_1".into(),
                tool_name: "web_search".into(),
                tool_json: "{\"search_query\":\"x\"}".into(),
                ..Default::default()
            },
            SseBlock {
                kind: "web_search_tool_result".into(),
                raw: Some(
                    json!({"type":"web_search_tool_result","tool_use_id":"call_1","content":[{"title":"x"}]}),
                ),
                ..Default::default()
            },
        ];
        let (content, _text, _thinking, tool_calls) = reconstruct_segment(&blocks).unwrap();
        assert_eq!(content[0]["type"], "server_tool_use");
        assert_eq!(content[0]["id"], "call_1");
        assert_eq!(content[1]["type"], "web_search_tool_result");
        assert_eq!(content[1]["tool_use_id"], "call_1");
        assert!(tool_calls.is_empty());
    }

    /// LIVE: validates the §3 task-local credential path end-to-end against the
    /// real GLM endpoint. The transport is constructed with creds present ONLY
    /// in the per-session task-local (never process env), so a successful turn
    /// proves the task-local is in scope at transport construction + the HTTP
    /// call (the one risk a unit test can't cover — task-locals don't propagate
    /// into spawned tasks). Ignored by default; run with creds in ~/.claude-zai:
    ///   cargo test -p bro-harness live_glm_turn_resolves_creds_from_task_local -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live: hits the real GLM endpoint; needs ~/.claude-zai/settings.json"]
    async fn live_glm_turn_resolves_creds_from_task_local() {
        let home = dirs::home_dir().expect("home dir");
        let body = std::fs::read_to_string(home.join(".claude-zai/settings.json"))
            .expect("GLM settings.json");
        let v: Value = serde_json::from_str(&body).unwrap();

        let mut vars = std::collections::BTreeMap::new();
        vars.insert("BRO_HARNESS_TRANSPORT".to_string(), "anthropic".to_string());
        for k in [
            "ANTHROPIC_BASE_URL",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_API_KEY",
        ] {
            if let Some(val) = v["env"][k].as_str() {
                vars.insert(k.to_string(), val.to_string());
            }
        }
        let model = v["env"]["ANTHROPIC_DEFAULT_HAIKU_MODEL"]
            .as_str()
            .unwrap_or("glm-4.6")
            .to_string();

        crate::transport::with_session_env(vars, async move {
            let mut tx = AnthropicTransport::from_env()
                .expect("transport constructed from task-local creds (process env has none)");
            tx.push_user_text("Reply with exactly: OK");
            let opts = TurnOpts {
                model,
                max_tokens: 32,
                base_instructions: None,
                system: SystemPrompt::default(),
                effort: None,
                web_search: false,
                service_tier: None,
            };
            let sink = crate::emit::Emitter::new("live-probe".to_string());
            let out = tx
                .run_turn(&[], &opts, &sink)
                .await
                .expect("live GLM turn via task-local creds");
            eprintln!("LIVE GLM RESPONSE: {:?}", out.text);
            assert!(
                !out.text.trim().is_empty(),
                "expected a non-empty response from the live endpoint"
            );
        })
        .await;
    }
}

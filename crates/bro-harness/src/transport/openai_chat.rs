//! OpenAI Chat Completions transport — the legacy fallback path that most
//! OpenAI-compatible endpoints speak (verified live against DeepSeek).
//!
//! Tools: `{"type":"function","function":{name,description,parameters}}`.
//! Assistant tool calls: `message.tool_calls[].{id,function:{name,arguments}}`
//! where `arguments` is a JSON *string*. Continue by appending the assistant
//! message verbatim, then one `{"role":"tool","tool_call_id","content"}` per
//! result. `finish_reason:"tool_calls"` means keep looping.
//!
//! Streaming: `stream:true` + `stream_options.include_usage`. The
//! `chat.completion.chunk` deltas are folded incrementally and re-emitted to
//! the [`TurnSink`](super::TurnSink) translated into the **Anthropic** event
//! shape (`content_block_delta` text/thinking, `content_block_start` +
//! `input_json_delta` for tool calls), so the harness speaks one convergent
//! stream-json protocol regardless of the underlying provider.
//!
//! Reasoning: reasoning-capable chat endpoints (Mistral, verified live
//! 2026-06-01) stream `delta.content` as a typed-chunk **array** —
//! `{type:"thinking", thinking:[{type:"text",text}]}` and `{type:"text",text}`
//! — once `reasoning_effort` is requested, and revert to a plain string after
//! the thinking block closes. The fold handles both forms: thinking text is
//! surfaced as the turn's display-only `thinking` (never replayed into the
//! buffer, matching the Anthropic transport), and visible text is replayed as
//! usual. The request-side `reasoning_effort` knob is provider-specific and
//! gated by [`ReasoningProfile`].

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

/// Request-side reasoning shaping for reasoning-capable chat endpoints.
///
/// Output-side parsing of array-form `content` (thinking chunks) is always on
/// and provider-agnostic — it costs nothing when the field is absent. The
/// *request* knob differs per provider, though: Mistral's `reasoning_effort`
/// only accepts `{none, high}` (verified live — `medium`/`low` 400 with
/// `invalid_request_invalid_args`), so a generic harness effort string is
/// collapsed into that set. Endpoints with no reasoning knob stay [`Off`] and
/// send no `reasoning_effort`. Selected via `BRO_HARNESS_CHAT_REASONING`.
///
/// [`Off`]: ReasoningProfile::Off
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum ReasoningProfile {
    #[default]
    Off,
    Mistral,
}

impl ReasoningProfile {
    fn from_env() -> Self {
        match std::env::var("BRO_HARNESS_CHAT_REASONING")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "mistral" => ReasoningProfile::Mistral,
            _ => ReasoningProfile::Off,
        }
    }

    /// Map a generic harness effort into this endpoint's accepted
    /// `reasoning_effort` value, or `None` to omit the param entirely.
    ///
    /// Follows the harness "no effort ⇒ no thinking" convention (mirrors the
    /// Anthropic transport): an unset effort omits the knob, which is also
    /// Mistral's no-reasoning default. Any effort that requests reasoning maps
    /// to `high`; an explicit off/none/minimal maps to `none`.
    fn reasoning_effort(self, effort: Option<&str>) -> Option<&'static str> {
        match self {
            ReasoningProfile::Off => None,
            ReasoningProfile::Mistral => match effort.map(str::trim) {
                None | Some("") => None,
                Some(e) => match e.to_ascii_lowercase().as_str() {
                    "off" | "none" | "minimal" => Some("none"),
                    _ => Some("high"),
                },
            },
        }
    }
}

pub struct OpenAiChatTransport {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    messages: Vec<Value>,
    reasoning: ReasoningProfile,
}

impl OpenAiChatTransport {
    pub fn from_env() -> Result<Self> {
        // Base carries the version segment: OpenAI = ".../v1", DeepSeek = root.
        let base_url = std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("ANTHROPIC_AUTH_TOKEN")) // DeepSeek: one key, both APIs
            .context("OPENAI_API_KEY not set")?;
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            messages: Vec::new(),
            reasoning: ReasoningProfile::from_env(),
        })
    }

    /// Build the Chat Completions request body (pure; no I/O), so the system
    /// split (leading stable + trailing volatile, neither persisted) is
    /// unit-testable.
    fn build_body(&self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Value {
        let tool_defs: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.schema,
                    }
                })
            })
            .collect();

        // System prompt is split: the cache-stable prefix is the leading system
        // message (so the prefix stays byte-identical and the endpoint's
        // automatic prefix cache holds); the volatile tail (manifest/nudges) is
        // a *trailing* system message after the conversation. Both are
        // per-request only (not stored in self.messages), so resume/edit stays
        // cheap and the volatile tail never persists into history.
        let mut msgs: Vec<Value> = Vec::with_capacity(self.messages.len() + 2);
        if let Some(stable) = opts.system.stable_text() {
            msgs.push(json!({"role": "system", "content": stable}));
        }
        msgs.extend(self.messages.iter().cloned());
        if let Some(volatile) = opts.system.volatile_text() {
            msgs.push(json!({"role": "system", "content": volatile}));
        }

        let mut body = json!({
            "model": opts.model,
            "messages": msgs,
            "max_tokens": opts.max_tokens,
            "stream": true,
            // Without this the streamed response omits the final usage chunk.
            "stream_options": {"include_usage": true},
        });
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
            body["tool_choice"] = json!("auto");
        }
        // Request-side reasoning knob, mapped to the endpoint's accepted set.
        // Output-side thinking parsing is unconditional (see run_turn), so a
        // model that reasons by default still surfaces thinking even with the
        // profile Off.
        if let Some(effort) = self.reasoning.reasoning_effort(opts.effort.as_deref()) {
            body["reasoning_effort"] = json!(effort);
        }
        body
    }

    /// One-shot, non-streaming summarization over `transcript` for compaction.
    /// Does NOT touch the conversation buffer — the caller swaps it afterward.
    async fn summarize_text(
        &self,
        transcript: &str,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<String> {
        let body = json!({
            "model": opts.model,
            "max_tokens": 2048,
            "messages": [
                {"role": "system", "content": "You summarize coding-agent conversations precisely and completely."},
                {"role": "user", "content": format!("{transcript}\n\n---\n{instruction}")},
            ],
        });
        let url = format!("{}/chat/completions", self.base_url);
        let resp = super::http::send_with_retry("openai-chat/compact", || {
            self.http
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", self.api_key))
                .timeout(super::http::request_timeout())
                .json(&body)
                .send()
        })
        .await
        .context("chat compaction request")?;
        let status = resp.status();
        let text = resp.text().await.context("read summarize body")?;
        if !status.is_success() {
            anyhow::bail!("openai chat compact {status}: {text}");
        }
        let v: Value = serde_json::from_str(&text).context("parse summarize response")?;
        let out = v["choices"][0]["message"]["content"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if out.trim().is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(out)
    }
}

/// One in-progress tool call accumulated across `chat.completion.chunk` deltas
/// (OpenAI fragments `function.arguments` across chunks; `id`/`name` usually
/// arrive only on the first fragment).
#[derive(Default)]
struct ChatToolAcc {
    id: String,
    name: String,
    args: String,
}

/// Accumulate a visible-text fragment into the turn buffer and forward it to the
/// sink as an Anthropic `text_delta` at block index 1 (thinking holds index 0),
/// opening the block on first use.
fn emit_text_delta(
    t: &str,
    text_out: &mut String,
    text_started: &mut bool,
    sink: &dyn super::TurnSink,
) {
    if !*text_started {
        sink.stream_event(json!({
            "type": "content_block_start",
            "index": 1,
            "content_block": {"type": "text", "text": ""},
        }));
        *text_started = true;
    }
    text_out.push_str(t);
    sink.stream_event(json!({
        "type": "content_block_delta",
        "index": 1,
        "delta": {"type": "text_delta", "text": t},
    }));
}

/// Flatten a Mistral `thinking` chunk payload to plain text. The payload is an
/// array of `{type:"text", text}` parts (occasionally a bare string); anything
/// else is ignored.
fn extract_thinking_text(thinking: &Value) -> String {
    if let Some(s) = thinking.as_str() {
        return s.to_string();
    }
    let Some(parts) = thinking.as_array() else {
        return String::new();
    };
    let mut out = String::new();
    for p in parts {
        if let Some(s) = p.as_str() {
            out.push_str(s);
        } else if p["type"].as_str() == Some("text") {
            out.push_str(p["text"].as_str().unwrap_or(""));
        }
    }
    out
}

#[async_trait]
impl Transport for OpenAiChatTransport {
    fn name(&self) -> &'static str {
        "openai-chat"
    }

    fn push_user_text(&mut self, text: &str) {
        self.messages.push(json!({"role": "user", "content": text}));
    }

    fn push_tool_results(&mut self, results: Vec<super::ToolResult>) {
        for r in results {
            self.messages.push(json!({
                "role": "tool",
                "tool_call_id": r.id,
                "content": r.content,
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

        let url = format!("{}/chat/completions", self.base_url);
        let resp = super::http::send_with_retry("openai-chat/completions", || {
            self.http
                .post(&url)
                .header("content-type", "application/json")
                .header("authorization", format!("Bearer {}", self.api_key))
                .timeout(super::http::request_timeout())
                .json(&body)
                .send()
        })
        .await
        .context("chat/completions request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai chat {status}: {text}");
        }

        // Fold the SSE chunk stream: forward text + tool deltas to the sink in
        // Anthropic shape, and reconstruct the OpenAI-native assistant message.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut text_out = String::new();
        let mut text_started = false;
        let mut reasoning_out = String::new();
        let mut thinking_started = false;
        let mut tools_acc: Vec<ChatToolAcc> = Vec::new();
        let mut finish: Option<String> = None;
        let mut usage = Usage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read chat SSE chunk")?;
            buf.extend_from_slice(&chunk);
            while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                let raw: Vec<u8> = buf.drain(..=pos).collect();
                let line = String::from_utf8_lossy(&raw);
                let line = line.trim_end();
                let Some(data) = line.strip_prefix("data:") else {
                    continue;
                };
                let data = data.trim();
                if data.is_empty() || data == "[DONE]" {
                    continue;
                }
                let Ok(ev) = serde_json::from_str::<Value>(data) else {
                    tracing::warn!("openai chat SSE parse skipped a line");
                    continue;
                };

                // Final usage chunk (include_usage) carries `usage` with empty
                // `choices`. `prompt_tokens` is cache-INCLUSIVE; subtract the
                // cached subset so `input_tokens` stays fresh.
                if ev["usage"].is_object() {
                    let total = ev["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                    let cached = ev["usage"]["prompt_tokens_details"]["cached_tokens"]
                        .as_u64()
                        .unwrap_or(0);
                    usage = Usage {
                        input_tokens: total.saturating_sub(cached),
                        output_tokens: ev["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                        cached_input_tokens: cached,
                        cache_creation_input_tokens: 0,
                    };
                }

                let choice = &ev["choices"][0];
                if let Some(fr) = choice["finish_reason"].as_str() {
                    finish = Some(fr.to_string());
                }
                let delta = &choice["delta"];

                // `delta.content` is either a plain string (no reasoning) or,
                // once reasoning is on, an array of typed chunks. A single
                // delta can mix an (empty) thinking chunk with a text chunk at
                // the thinking→text transition, so iterate every chunk.
                // Block indices: thinking=0, text=1, tool_use=tc_index+2. No
                // consumer keys off the literal index (the daemon parser groups
                // by type, not index; the fleet renders from the final
                // assistant block), so the gap when reasoning is off is inert —
                // this layout just mirrors Claude's native block ordering.
                match &delta["content"] {
                    Value::String(t) if !t.is_empty() => {
                        emit_text_delta(t, &mut text_out, &mut text_started, sink);
                    }
                    Value::Array(chunks) => {
                        for chunk in chunks {
                            match chunk["type"].as_str() {
                                Some("thinking") => {
                                    let tt = extract_thinking_text(&chunk["thinking"]);
                                    if tt.is_empty() {
                                        continue;
                                    }
                                    if !thinking_started {
                                        sink.stream_event(json!({
                                            "type": "content_block_start",
                                            "index": 0,
                                            "content_block": {"type": "thinking", "thinking": ""},
                                        }));
                                        thinking_started = true;
                                    }
                                    reasoning_out.push_str(&tt);
                                    sink.stream_event(json!({
                                        "type": "content_block_delta",
                                        "index": 0,
                                        "delta": {"type": "thinking_delta", "thinking": tt},
                                    }));
                                }
                                Some("text") => {
                                    if let Some(t) = chunk["text"].as_str()
                                        && !t.is_empty()
                                    {
                                        emit_text_delta(t, &mut text_out, &mut text_started, sink);
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }

                if let Some(tcs) = delta["tool_calls"].as_array() {
                    for tc in tcs {
                        let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                        while tools_acc.len() <= idx {
                            tools_acc.push(ChatToolAcc::default());
                        }
                        let acc = &mut tools_acc[idx];
                        if let Some(id) = tc["id"].as_str()
                            && !id.is_empty()
                        {
                            acc.id = id.to_string();
                        }
                        if let Some(name) = tc["function"]["name"].as_str()
                            && !name.is_empty()
                        {
                            acc.name = name.to_string();
                            // Tool blocks live after thinking(0) + text(1).
                            sink.stream_event(json!({
                                "type": "content_block_start",
                                "index": idx + 2,
                                "content_block": {"type": "tool_use", "id": acc.id, "name": acc.name},
                            }));
                        }
                        if let Some(frag) = tc["function"]["arguments"].as_str()
                            && !frag.is_empty()
                        {
                            acc.args.push_str(frag);
                            sink.stream_event(json!({
                                "type": "content_block_delta",
                                "index": idx + 2,
                                "delta": {"type": "input_json_delta", "partial_json": frag},
                            }));
                        }
                    }
                }
            }
        }

        // Reconstruct the OpenAI-native assistant message for the next request.
        let mut assistant = json!({"role": "assistant"});
        assistant["content"] = if text_out.is_empty() {
            Value::Null
        } else {
            json!(text_out)
        };
        let mut tool_calls: Vec<super::ToolCall> = Vec::new();
        let native_tcs: Vec<Value> = tools_acc
            .iter()
            .filter(|a| !a.id.is_empty() || !a.name.is_empty())
            .map(|a| {
                let args_str = if a.args.is_empty() { "{}" } else { &a.args };
                tool_calls.push(super::ToolCall {
                    id: a.id.clone(),
                    name: a.name.clone(),
                    args: serde_json::from_str(args_str).unwrap_or(json!({})),
                });
                json!({
                    "id": a.id,
                    "type": "function",
                    "function": {"name": a.name, "arguments": args_str},
                })
            })
            .collect();
        if !native_tcs.is_empty() {
            assistant["tool_calls"] = json!(native_tcs);
        }
        self.messages.push(assistant);

        let mut stop = match finish.as_deref() {
            Some("tool_calls") => StopReason::ToolCalls,
            Some("stop") => StopReason::Done,
            Some("length") => StopReason::Length,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::Done,
        };
        if !tool_calls.is_empty() {
            stop = StopReason::ToolCalls;
        }

        Ok(TurnOutput {
            text: text_out,
            // Display-only: thinking is surfaced for the assistant turn block
            // but never replayed into `self.messages` (the assistant message
            // above carries text + tool_calls only), matching the Anthropic
            // transport and keeping multi-turn requests reasoning-free.
            thinking: reasoning_out,
            tool_calls,
            stop,
            usage,
        })
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
        // A turn cancelled mid-stream leaves a trailing user message with no
        // assistant reply; the next turn would then send two user messages in a
        // row. Append a synthetic assistant turn to keep alternation valid. (A
        // trailing `tool` message is left as-is — OpenAI accepts a user message
        // after tool output.)
        if self.messages.last().and_then(|m| m["role"].as_str()) == Some("user") {
            self.messages.push(json!({
                "role": "assistant",
                "content": super::INTERRUPT_ASSISTANT_MARKER,
            }));
        }
    }

    async fn compact(
        &mut self,
        keep_tail: usize,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<Option<String>> {
        let n = self.messages.len();
        if n <= keep_tail + 1 {
            return Ok(None);
        }
        let limit = n.saturating_sub(keep_tail);
        let Some(split) = chat_split(&self.messages, limit) else {
            return Ok(None);
        };
        let transcript = render_chat_transcript(&self.messages[..split]);
        let summary = self.summarize_text(&transcript, instruction, opts).await?;
        let mut rebuilt: Vec<Value> = Vec::with_capacity(n - split + 1);
        rebuilt.push(json!({
            "role": "user",
            "content": format!("[Earlier conversation compacted to a summary]\n\n{summary}"),
        }));
        rebuilt.extend_from_slice(&self.messages[split..]);
        self.messages = rebuilt;
        Ok(Some(summary))
    }
}

/// Largest split `≤ limit` whose kept tail begins on an `assistant` message, so
/// no `tool` message is orphaned from the `tool_calls` that produced it. Returns
/// `None` when no such boundary exists.
fn chat_split(messages: &[Value], limit: usize) -> Option<usize> {
    (1..limit)
        .rev()
        .find(|&i| messages[i]["role"].as_str() == Some("assistant"))
}

/// Render a slice of the chat buffer to a plain-text transcript for
/// summarization. Tool results are truncated to keep the prompt bounded.
fn render_chat_transcript(messages: &[Value]) -> String {
    let mut s = String::new();
    for m in messages {
        let role = m["role"].as_str().unwrap_or("?");
        s.push_str(&format!("\n## {role}\n"));
        if let Some(t) = m["content"].as_str() {
            if role == "tool" {
                s.push_str(&super::truncate(t, 2000));
            } else {
                s.push_str(t);
            }
            s.push('\n');
        }
        if let Some(tcs) = m["tool_calls"].as_array() {
            for tc in tcs {
                s.push_str(&format!(
                    "[tool_call {} {}]\n",
                    tc["function"]["name"].as_str().unwrap_or(""),
                    tc["function"]["arguments"].as_str().unwrap_or("")
                ));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;

    fn transport() -> OpenAiChatTransport {
        OpenAiChatTransport {
            http: reqwest::Client::new(),
            base_url: "http://x".into(),
            api_key: "k".into(),
            messages: vec![json!({"role": "user", "content": "hi"})],
            reasoning: ReasoningProfile::Off,
        }
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "m".into(),
            max_tokens: 16,
            system,
            effort: None,
            web_search: false,
        }
    }

    #[test]
    fn stable_leads_volatile_trails_conversation() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("MANIFEST".into()),
            }),
        );
        let msgs = body["messages"].as_array().unwrap();
        // [ system(stable), user(hi), system(volatile) ] — stable stays the
        // byte-stable prefix; volatile rides the tail.
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "system");
        assert_eq!(msgs[0]["content"], "BASE");
        assert_eq!(msgs[1]["role"], "user");
        assert_eq!(msgs[2]["role"], "system");
        assert_eq!(msgs[2]["content"], "MANIFEST");
        // Streaming is requested with usage included.
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
        // Neither system message was persisted into the buffer.
        assert_eq!(transport().messages.len(), 1);
    }

    #[test]
    fn no_volatile_means_no_trailing_system() {
        let body = transport().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: None,
            }),
        );
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["content"], "BASE");
        assert_eq!(msgs[1]["role"], "user");
    }

    #[test]
    fn chat_split_keeps_tail_on_assistant() {
        let msgs = vec![
            json!({"role": "user", "content": "go"}),
            json!({"role": "assistant", "tool_calls": [{"id": "a"}]}),
            json!({"role": "tool", "tool_call_id": "a", "content": "r"}),
            json!({"role": "assistant", "content": "done"}),
        ];
        // Within limit 3, the newest assistant boundary is index 1 (a `tool`
        // start at 2 would orphan its call).
        assert_eq!(chat_split(&msgs, 3), Some(1));
        // No assistant before limit 1 → nothing safe to compact.
        assert_eq!(chat_split(&msgs, 1), None);
    }

    #[test]
    fn reasoning_profile_off_never_sends_effort() {
        let p = ReasoningProfile::Off;
        assert_eq!(p.reasoning_effort(None), None);
        assert_eq!(p.reasoning_effort(Some("high")), None);
        assert_eq!(p.reasoning_effort(Some("medium")), None);
    }

    #[test]
    fn reasoning_profile_mistral_collapses_to_none_or_high() {
        let p = ReasoningProfile::Mistral;
        // Unset / blank ⇒ omit (Mistral's no-reasoning default; mirrors the
        // "no effort ⇒ no thinking" convention).
        assert_eq!(p.reasoning_effort(None), None);
        assert_eq!(p.reasoning_effort(Some("")), None);
        assert_eq!(p.reasoning_effort(Some("  ")), None);
        // Explicit off ⇒ "none".
        assert_eq!(p.reasoning_effort(Some("off")), Some("none"));
        assert_eq!(p.reasoning_effort(Some("none")), Some("none"));
        assert_eq!(p.reasoning_effort(Some("minimal")), Some("none"));
        // Mistral rejects medium/low (verified live) → collapse to the only
        // reasoning-on value it accepts.
        assert_eq!(p.reasoning_effort(Some("low")), Some("high"));
        assert_eq!(p.reasoning_effort(Some("medium")), Some("high"));
        assert_eq!(p.reasoning_effort(Some("high")), Some("high"));
        assert_eq!(p.reasoning_effort(Some("max")), Some("high"));
        assert_eq!(p.reasoning_effort(Some("HIGH")), Some("high"));
    }

    #[test]
    fn build_body_adds_reasoning_effort_only_under_profile() {
        // Off profile: no reasoning_effort even when an effort is set.
        let mut tx = transport();
        tx.reasoning = ReasoningProfile::Off;
        let mut o = opts(SystemPrompt::default());
        o.effort = Some("high".into());
        assert!(tx.build_body(&[], &o).get("reasoning_effort").is_none());

        // Mistral profile: maps and sends.
        tx.reasoning = ReasoningProfile::Mistral;
        let body = tx.build_body(&[], &o);
        assert_eq!(body["reasoning_effort"], "high");

        // Mistral profile, no effort: omit.
        let o_none = opts(SystemPrompt::default());
        assert!(
            tx.build_body(&[], &o_none)
                .get("reasoning_effort")
                .is_none()
        );
    }

    #[test]
    fn extract_thinking_text_handles_chunk_shapes() {
        // The live shape: thinking is an array of {type:"text", text}.
        assert_eq!(
            extract_thinking_text(&json!([{"type": "text", "text": "Okay"}])),
            "Okay"
        );
        assert_eq!(
            extract_thinking_text(
                &json!([{"type": "text", "text": "a"}, {"type": "text", "text": "b"}])
            ),
            "ab"
        );
        // Empty array (the thinking→text transition chunk) yields nothing.
        assert_eq!(extract_thinking_text(&json!([])), "");
        // Bare-string fallback.
        assert_eq!(extract_thinking_text(&json!("plain")), "plain");
        // Non-text parts ignored.
        assert_eq!(extract_thinking_text(&json!([{"type": "image"}])), "");
        assert_eq!(extract_thinking_text(&Value::Null), "");
    }
}

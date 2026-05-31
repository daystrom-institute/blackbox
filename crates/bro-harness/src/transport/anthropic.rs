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
            "messages": self.messages,
            "stream": true,
        });
        // System is up to two blocks: a cache-stable prefix (carries the
        // ephemeral cache breakpoint) and a volatile tail (manifest/nudges,
        // never cached, so a changing tail can't invalidate the cached prefix).
        let mut system_blocks: Vec<Value> = Vec::new();
        if let Some(stable) = opts.system.stable_text() {
            system_blocks.push(json!({
                "type": "text", "text": stable, "cache_control": {"type": "ephemeral"},
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
        if let Some(t) = effort_to_thinking(opts.effort.as_deref()) {
            body["thinking"] = t;
        }
        body
    }

    /// One-shot, non-streaming summarization over `transcript`. Does NOT touch
    /// the conversation buffer — the caller swaps the buffer afterward.
    async fn summarize_text(
        &self,
        transcript: &str,
        instruction: &str,
        opts: &TurnOpts,
    ) -> Result<String> {
        let body = json!({
            "model": opts.model,
            "max_tokens": 2048,
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
        if out.trim().is_empty() {
            anyhow::bail!("compaction summary was empty");
        }
        Ok(out)
    }
}

/// One in-progress content block accumulated from SSE deltas.
#[derive(Default)]
struct SseBlock {
    /// `"text"`, `"thinking"`, `"tool_use"`, or other.
    kind: String,
    /// Accumulated `text_delta.text` or `thinking_delta.thinking`.
    text: String,
    tool_id: String,
    tool_name: String,
    /// Accumulated `input_json_delta.partial_json`.
    tool_json: String,
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
            if b.kind == "tool_use" {
                b.tool_id = cb["id"].as_str().unwrap_or("").to_string();
                b.tool_name = cb["name"].as_str().unwrap_or("").to_string();
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
fn render_transcript(messages: &[Value]) -> String {
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
                            truncate(b["content"].as_str().unwrap_or(""), 2000)
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
        let body = self.build_body(tools, opts);

        let url = format!("{}/v1/messages", self.base_url);
        let resp = super::http::send_with_retry("anthropic/messages", || {
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
        .context("messages request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("anthropic messages {status}: {text}");
        }

        // Parse the SSE stream incrementally: forward each event to the sink and
        // fold it into the normalized turn output. Buffer raw bytes and decode
        // only complete lines so a multibyte char split across chunks is safe.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut blocks: Vec<SseBlock> = Vec::new();
        let mut usage = Usage::default();
        let mut stop = StopReason::Done;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("read messages SSE chunk")?;
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
                        sink.stream_event(ev.clone());
                        fold_sse(&ev, &mut blocks, &mut usage, &mut stop);
                    }
                    Err(e) => tracing::warn!("anthropic SSE parse error: {e}"),
                }
            }
        }

        // Reconstruct the assistant turn from the accumulated blocks.
        let mut content: Vec<Value> = Vec::new();
        let mut text_out = String::new();
        let mut tool_calls: Vec<super::ToolCall> = Vec::new();
        for b in &blocks {
            match b.kind.as_str() {
                "text" if !b.text.is_empty() => {
                    content.push(json!({"type": "text", "text": b.text}));
                    text_out.push_str(&b.text);
                }
                "tool_use" => {
                    let args: Value = serde_json::from_str(if b.tool_json.is_empty() {
                        "{}"
                    } else {
                        &b.tool_json
                    })
                    .unwrap_or_else(|_| json!({}));
                    content.push(json!({
                        "type": "tool_use",
                        "id": b.tool_id,
                        "name": b.tool_name,
                        "input": args.clone(),
                    }));
                    tool_calls.push(super::ToolCall {
                        id: b.tool_id.clone(),
                        name: b.tool_name.clone(),
                        args,
                    });
                }
                // `thinking` blocks are intentionally NOT replayed: Anthropic
                // requires a matching signature to send a thinking block back,
                // and we don't persist one. The live thinking already streamed
                // to the sink; dropping it from the buffer keeps resume valid.
                _ => {}
            }
        }

        // Record the assistant turn for the next request. Always store the
        // assistant message (even if empty) so the conversation alternates
        // correctly when only tool calls were produced.
        self.messages
            .push(json!({"role": "assistant", "content": content}));

        // Safety: if tool calls were produced, ensure the loop continues even
        // if the stop_reason event was missed.
        if !tool_calls.is_empty() {
            stop = StopReason::ToolCalls;
        }

        Ok(TurnOutput {
            text: text_out,
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

        let transcript = render_transcript(&self.messages[..split]);
        let summary = self.summarize_text(&transcript, instruction, opts).await?;

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

fn effort_to_thinking(effort: Option<&str>) -> Option<Value> {
    let budget = match effort?.to_ascii_lowercase().as_str() {
        "low" => 2048,
        "medium" => 8192,
        "high" | "max" => 16384,
        _ => return None,
    };
    Some(json!({"type": "enabled", "budget_tokens": budget}))
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
}

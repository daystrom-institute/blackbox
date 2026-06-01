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

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiResponsesTransport {
    http: reqwest::Client,
    endpoint: String,
    auth: Auth,
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
            input: Vec::new(),
        })
    }
}

#[async_trait]
impl Transport for OpenAiResponsesTransport {
    fn name(&self) -> &'static str {
        "openai-responses"
    }

    fn push_user_text(&mut self, text: &str) {
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

        let resp = super::http::send_with_retry("openai-responses", || {
            let mut rb = self
                .http
                .post(&self.endpoint)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .timeout(super::http::request_timeout());
            rb = match &self.auth {
                Auth::ApiKey(k) => rb.header("authorization", format!("Bearer {k}")),
                Auth::ChatGpt {
                    access_token,
                    account_id,
                } => rb
                    .header("authorization", format!("Bearer {access_token}"))
                    .header("chatgpt-account-id", account_id.clone())
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("originator", "codex_cli_rs")
                    .header("session_id", uuid::Uuid::new_v4().to_string()),
            };
            rb.json(&body).send()
        })
        .await
        .context("responses request")?;
        let status = resp.status();
        if !status.is_success() {
            let sse = resp.text().await.unwrap_or_default();
            anyhow::bail!("openai responses {status}: {sse}");
        }

        // Stream the SSE: forward text/reasoning deltas to the sink live (in
        // Anthropic shape) while accumulating the full body, then hand it to the
        // proven `parse_sse` for the authoritative item/usage reconstruction.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        let mut accum = String::new();
        let mut text_started = false;
        while let Some(chunk) = stream.next().await {
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
        if let Some(e) = &opts.effort {
            body["reasoning"] = json!({"effort": normalize_effort(e)});
        }
        body
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
                    }
                }
                "response.failed" | "error" => {
                    anyhow::bail!("responses stream error: {data}");
                }
                _ => {}
            }
        }

        // Echo the model's output items back into the buffer for continuity —
        // EXCEPT reasoning items. With `store:false` (required by the ChatGPT
        // backend) reasoning items (`rs_…`) are not persisted server-side, so
        // replaying one by reference on the next turn 404s ("Item with id …
        // not found"). Dropping them costs cross-turn reasoning continuity but
        // keeps multi-turn (tool-calling) requests valid. To preserve it later,
        // request `include:["reasoning.encrypted_content"]` and replay only
        // items carrying `encrypted_content`.
        self.input.extend(
            output_items
                .iter()
                .filter(|item| item["type"].as_str() != Some("reasoning"))
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
            let mut rb = self
                .http
                .post(&self.endpoint)
                .header("content-type", "application/json")
                .header("accept", "text/event-stream")
                .timeout(super::http::request_timeout());
            rb = match &self.auth {
                Auth::ApiKey(k) => rb.header("authorization", format!("Bearer {k}")),
                Auth::ChatGpt {
                    access_token,
                    account_id,
                } => rb
                    .header("authorization", format!("Bearer {access_token}"))
                    .header("chatgpt-account-id", account_id.clone())
                    .header("OpenAI-Beta", "responses=experimental")
                    .header("originator", "codex_cli_rs")
                    .header("session_id", uuid::Uuid::new_v4().to_string()),
            };
            rb.json(&body).send()
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

fn normalize_effort(e: &str) -> &str {
    match e.to_ascii_lowercase().as_str() {
        "low" => "low",
        "high" | "max" => "high",
        _ => "medium",
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
            input: vec![json!({
                "type": "message", "role": "user",
                "content": [{"type": "input_text", "text": "hi"}],
            })],
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

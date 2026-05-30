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
//! shape (`content_block_delta` text, `content_block_start`/`input_json_delta`
//! for tool calls), so the harness speaks one convergent stream-json protocol
//! regardless of the underlying provider.

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use futures_util::StreamExt;
use serde_json::{Value, json};

pub struct OpenAiChatTransport {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    messages: Vec<Value>,
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
        body
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

                if let Some(t) = delta["content"].as_str()
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
                    text_out.push_str(t);
                    sink.stream_event(json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": {"type": "text_delta", "text": t},
                    }));
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
                            // Tool block lives after the text block (index 0).
                            sink.stream_event(json!({
                                "type": "content_block_start",
                                "index": idx + 1,
                                "content_block": {"type": "tool_use", "id": acc.id, "name": acc.name},
                            }));
                        }
                        if let Some(frag) = tc["function"]["arguments"].as_str()
                            && !frag.is_empty()
                        {
                            acc.args.push_str(frag);
                            sink.stream_event(json!({
                                "type": "content_block_delta",
                                "index": idx + 1,
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
}

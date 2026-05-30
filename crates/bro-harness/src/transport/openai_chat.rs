//! OpenAI Chat Completions transport — the broad-compatibility fallback for
//! OpenAI-style endpoints (DeepSeek's native API and most third-party
//! gateways). Tool calls use the `tools`/`tool_calls` schema; the assistant's
//! `tool_calls` are normalized to our `ToolCall`, results go back as `role:
//! "tool"` messages.
//!
//! Non-streaming for now: it satisfies the [`TurnSink`](super::TurnSink) seam
//! by ignoring the sink and emitting the whole assistant turn at the loop
//! level. Incremental SSE (`stream: true` + `chat.completion.chunk` deltas) is
//! the immediate follow-on, mirroring the anthropic transport.

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct OpenAiChatTransport {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    messages: Vec<Value>,
}

impl OpenAiChatTransport {
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("OPENAI_BASE_URL")
            .or_else(|_| std::env::var("OPENAI_API_BASE"))
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();
        let api_key = std::env::var("OPENAI_API_KEY")
            .or_else(|_| std::env::var("OPENAI_KEY"))
            .context("OPENAI_API_KEY not set")?;
        Ok(Self {
            http: reqwest::Client::new(),
            base_url,
            api_key,
            messages: Vec::new(),
        })
    }

    fn build_body(&self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Value {
        let mut msgs: Vec<Value> = Vec::new();
        if let Some(stable) = opts.system.stable_text() {
            msgs.push(json!({"role": "system", "content": stable}));
        }
        // The volatile tail (manifest/nudges) rides as a second system message.
        if let Some(volatile) = opts.system.volatile_text() {
            msgs.push(json!({"role": "system", "content": volatile}));
        }
        msgs.extend(self.messages.iter().cloned());

        let mut tool_defs: Vec<Value> = Vec::new();
        for t in tools {
            tool_defs.push(json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                },
            }));
        }

        let mut body = json!({
            "model": opts.model,
            "max_tokens": opts.max_tokens,
            "messages": msgs,
        });
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
        }
        body
    }
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
        _sink: &dyn super::TurnSink,
    ) -> Result<TurnOutput> {
        let body = self.build_body(tools, opts);
        let url = format!("{}/chat/completions", self.base_url);
        let resp = super::http::send_with_retry("openai/chat", || {
            self.http
                .post(&url)
                .header("authorization", format!("Bearer {}", self.api_key))
                .header("content-type", "application/json")
                .timeout(super::http::request_timeout())
                .json(&body)
                .send()
        })
        .await
        .context("chat completions request")?;
        let status = resp.status();
        let text = resp.text().await.context("read chat body")?;
        if !status.is_success() {
            anyhow::bail!("openai chat {status}: {text}");
        }
        let v: Value = serde_json::from_str(&text).context("parse chat response")?;
        let choice = &v["choices"][0];
        let msg = &choice["message"];

        // Record the assistant message verbatim for the next request.
        self.messages.push(msg.clone());

        let text_out = msg["content"].as_str().unwrap_or("").to_string();
        let tool_calls = msg["tool_calls"]
            .as_array()
            .map(|calls| {
                calls
                    .iter()
                    .filter_map(|c| {
                        Some(super::ToolCall {
                            id: c["id"].as_str()?.to_string(),
                            name: c["function"]["name"].as_str()?.to_string(),
                            args: serde_json::from_str(
                                c["function"]["arguments"].as_str().unwrap_or("{}"),
                            )
                            .unwrap_or(json!({})),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let stop = match choice["finish_reason"].as_str() {
            Some("tool_calls") => StopReason::ToolCalls,
            Some("stop") => StopReason::Done,
            Some("length") => StopReason::Length,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::Done,
        };
        let usage = Usage {
            input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: 0,
            cache_creation_input_tokens: 0,
        };

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

//! OpenAI Chat Completions transport — the legacy fallback path that most
//! OpenAI-compatible endpoints speak (verified live against DeepSeek).
//!
//! Tools: `{"type":"function","function":{name,description,parameters}}`.
//! Assistant tool calls: `message.tool_calls[].{id,function:{name,arguments}}`
//! where `arguments` is a JSON *string*. Continue by appending the assistant
//! message verbatim, then one `{"role":"tool","tool_call_id","content"}` per
//! result. `finish_reason:"tool_calls"` means keep looping.

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

    async fn run_turn(&mut self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Result<TurnOutput> {
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

        // System prompt is a leading system message (prepend once per request,
        // not stored, so resume/edit of system is cheap).
        let mut msgs: Vec<Value> = Vec::with_capacity(self.messages.len() + 1);
        if let Some(sys) = &opts.system {
            msgs.push(json!({"role": "system", "content": sys}));
        }
        msgs.extend(self.messages.iter().cloned());

        let mut body = json!({
            "model": opts.model,
            "messages": msgs,
            "max_tokens": opts.max_tokens,
        });
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
            body["tool_choice"] = json!("auto");
        }

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
        let text = resp.text().await.context("read body")?;
        if !status.is_success() {
            anyhow::bail!("openai chat {status}: {text}");
        }
        let v: Value = serde_json::from_str(&text).context("parse chat response")?;
        let choice = &v["choices"][0];
        let message = &choice["message"];

        // Record assistant message verbatim (incl. tool_calls) for next turn.
        self.messages.push(message.clone());

        let text_out = message["content"].as_str().unwrap_or("").to_string();
        let tool_calls = message["tool_calls"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|tc| {
                let args_str = tc["function"]["arguments"].as_str().unwrap_or("{}");
                Some(super::ToolCall {
                    id: tc["id"].as_str()?.to_string(),
                    name: tc["function"]["name"].as_str()?.to_string(),
                    args: serde_json::from_str(args_str).unwrap_or(json!({})),
                })
            })
            .collect::<Vec<_>>();

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

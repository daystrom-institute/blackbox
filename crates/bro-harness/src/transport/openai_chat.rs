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
        });
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
            body["tool_choice"] = json!("auto");
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

    async fn run_turn(&mut self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Result<TurnOutput> {
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
        // OpenAI Chat `prompt_tokens` is cache-INCLUSIVE; the cached subset is
        // in `prompt_tokens_details.cached_tokens`. Subtract it so
        // `input_tokens` stays fresh (cache-exclusive).
        let total_input = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
        let cached = v["usage"]["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let usage = Usage {
            input_tokens: total_input.saturating_sub(cached),
            output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
            cached_input_tokens: cached,
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

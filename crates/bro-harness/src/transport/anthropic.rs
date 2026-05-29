//! Anthropic Messages transport. Clean request body — none of the
//! schema-violating CLI scaffolding that broke GLM/DeepSeek. Responses parsed
//! loosely (content blocks kept raw) so provider drift never breaks decoding.
//! Non-streaming first cut; SSE is a later enhancement (the daemon's
//! `parse_claude_event` falls back to the assistant body anyway).

use super::{StopReason, Transport, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use async_trait::async_trait;
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
        self.messages.push(json!({"role": "user", "content": blocks}));
    }

    async fn run_turn(&mut self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Result<TurnOutput> {
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
            "stream": false,
        });
        if let Some(sys) = &opts.system {
            body["system"] = json!([{
                "type": "text", "text": sys, "cache_control": {"type": "ephemeral"},
            }]);
        }
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
        }
        if let Some(t) = effort_to_thinking(opts.effort.as_deref()) {
            body["thinking"] = t;
        }

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
        let text = resp.text().await.context("read messages body")?;
        if !status.is_success() {
            anyhow::bail!("anthropic messages {status}: {text}");
        }
        let v: Value = serde_json::from_str(&text).context("parse messages response")?;
        let content = v["content"].as_array().cloned().unwrap_or_default();

        // Record the assistant turn verbatim for the next request.
        self.messages
            .push(json!({"role": "assistant", "content": content.clone()}));

        let text_out = content
            .iter()
            .filter(|b| b["type"] == "text")
            .filter_map(|b| b["text"].as_str())
            .collect::<Vec<_>>()
            .join("");
        // `tool_use` only — `server_tool_use` (web search) is resolved upstream.
        let tool_calls = content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .filter_map(|b| {
                Some(super::ToolCall {
                    id: b["id"].as_str()?.to_string(),
                    name: b["name"].as_str()?.to_string(),
                    args: b.get("input").cloned().unwrap_or(json!({})),
                })
            })
            .collect::<Vec<_>>();

        let stop = match v["stop_reason"].as_str() {
            Some("tool_use") => StopReason::ToolCalls,
            Some("end_turn") | Some("stop_sequence") => StopReason::Done,
            Some("max_tokens") => StopReason::Length,
            Some(other) => StopReason::Other(other.to_string()),
            None => StopReason::Done,
        };
        let usage = Usage {
            input_tokens: v["usage"]["input_tokens"].as_u64().unwrap_or(0),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
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

fn effort_to_thinking(effort: Option<&str>) -> Option<Value> {
    let budget = match effort?.to_ascii_lowercase().as_str() {
        "low" => 2048,
        "medium" => 8192,
        "high" | "max" => 16384,
        _ => return None,
    };
    Some(json!({"type": "enabled", "budget_tokens": budget}))
}

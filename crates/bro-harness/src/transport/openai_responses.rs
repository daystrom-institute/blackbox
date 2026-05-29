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
    ChatGpt { access_token: String, account_id: String },
}

impl OpenAiResponsesTransport {
    pub fn from_env() -> Result<Self> {
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
        // Fall back to Codex ChatGPT OAuth.
        let (access_token, account_id) = codex_chatgpt_auth()
            .context("no OPENAI_API_KEY and could not load Codex ChatGPT auth")?;
        let endpoint = std::env::var("OPENAI_RESPONSES_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/responses".to_string());
        Ok(Self {
            http,
            endpoint,
            auth: Auth::ChatGpt {
                access_token,
                account_id,
            },
            input: Vec::new(),
        })
    }
}

/// Read `~/.codex/auth.json` (or `$CODEX_HOME/auth.json`) for the ChatGPT
/// access token + account id. NOTE: tokens expire; refresh is the Codex CLI's
/// job — we do not implement the refresh flow here (a daemon-side concern).
fn codex_chatgpt_auth() -> Result<(String, String)> {
    let home = std::env::var("CODEX_HOME").unwrap_or_else(|_| {
        dirs::home_dir()
            .map(|h| h.join(".codex").to_string_lossy().into_owned())
            .unwrap_or_default()
    });
    let path = std::path::Path::new(&home).join("auth.json");
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("read {}", path.display()))?;
    let v: Value = serde_json::from_str(&body).context("parse auth.json")?;
    let access = v["tokens"]["access_token"]
        .as_str()
        .context("auth.json missing tokens.access_token")?
        .to_string();
    let account = v["tokens"]["account_id"]
        .as_str()
        .context("auth.json missing tokens.account_id")?
        .to_string();
    Ok((access, account))
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

    async fn run_turn(&mut self, tools: &[super::ToolSpec], opts: &TurnOpts) -> Result<TurnOutput> {
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

        let mut body = json!({
            "model": opts.model,
            "input": self.input,
            "tool_choice": "auto",
            "parallel_tool_calls": false,
            "stream": true,
            "store": false,
        });
        // The ChatGPT backend rejects an empty/missing `instructions` field
        // ("Instructions are required"); always send a non-empty value.
        let instructions = opts
            .system
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("You are a helpful coding assistant operating non-interactively.");
        body["instructions"] = json!(instructions);
        if !tool_defs.is_empty() {
            body["tools"] = json!(tool_defs);
        }
        if let Some(e) = &opts.effort {
            body["reasoning"] = json!({"effort": normalize_effort(e)});
        }

        let mut rb = self
            .http
            .post(&self.endpoint)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
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

        let resp = rb.json(&body).send().await.context("responses request")?;
        let status = resp.status();
        let sse = resp.text().await.context("read responses body")?;
        if !status.is_success() {
            anyhow::bail!("openai responses {status}: {sse}");
        }
        self.parse_sse(&sse)
    }

    fn snapshot(&self) -> Value {
        json!(self.input)
    }
    fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.input = arr.clone();
        }
    }
}

impl OpenAiResponsesTransport {
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
                    usage = Usage {
                        input_tokens: r["usage"]["input_tokens"].as_u64().unwrap_or(0),
                        output_tokens: r["usage"]["output_tokens"].as_u64().unwrap_or(0),
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

        // Echo the model's output items back into the buffer for continuity.
        self.input.extend(output_items.iter().cloned());

        let mut text = String::new();
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
            tool_calls,
            stop,
            usage,
        })
    }
}

fn normalize_effort(e: &str) -> &str {
    match e.to_ascii_lowercase().as_str() {
        "low" => "low",
        "high" | "max" => "high",
        _ => "medium",
    }
}

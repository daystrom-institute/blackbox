//! Transport-agnostic building blocks shared by the OpenAI Responses transports
//! (HTTP-SSE today; a WebSocket transport next — see
//! `design/bro-harness/brodex-websocket-transport.md`).
//!
//! Everything here is independent of the wire/connection mechanism: the auth
//! enum + resolution, the request-body builder, the SSE/event reconstruction,
//! the identity+auth header *values* (each transport applies them to its own
//! request type — reqwest builder vs WS handshake), and the small pure mappers.
//! Each transport owns only its connection lifecycle and framing.

use super::{StopReason, ToolCall, ToolResult, ToolSpec, TurnOpts, TurnOutput, Usage};
use anyhow::{Context, Result};
use serde_json::{Value, json};

/// Auth material for a Responses request.
pub(super) enum Auth {
    /// Standard OpenAI: `Authorization: Bearer <key>`.
    ApiKey(String),
    /// ChatGPT backend: bearer access token + account id.
    ChatGpt {
        access_token: String,
        account_id: String,
    },
}

/// The conversation + identity state shared by the HTTP-SSE and WebSocket
/// Responses transports. Owning this in one place lets the routing transport
/// keep a single buffer across a WS→HTTP fallback (no two-buffer divergence).
pub(super) struct ResponsesState {
    pub auth: Auth,
    /// Stable per-session id (codex `session-id` header + `prompt_cache_key`).
    pub session_id: String,
    /// Per-turn id (codex `thread-id` header); rotated on each new user turn.
    pub thread_id: String,
    /// Flat Responses `input[]` buffer.
    pub input: Vec<Value>,
}

impl ResponsesState {
    pub(super) fn new(auth: Auth) -> Self {
        Self {
            auth,
            session_id: String::new(),
            thread_id: new_id(),
            input: Vec::new(),
        }
    }

    pub(super) fn push_user_text(&mut self, text: &str) {
        // A new user turn begins: rotate the per-turn `thread-id` (codex mints a
        // fresh ThreadId per turn while `session-id` stays stable).
        self.thread_id = new_id();
        self.input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": text}],
        }));
    }

    pub(super) fn push_tool_results(&mut self, results: Vec<ToolResult>) {
        for r in results {
            self.input.push(json!({
                "type": "function_call_output",
                "call_id": r.id,
                "output": r.content,
            }));
        }
    }

    pub(super) fn snapshot(&self) -> Value {
        json!(self.input)
    }

    pub(super) fn restore(&mut self, snapshot: Value) {
        if let Some(arr) = snapshot.as_array() {
            self.input = arr.clone();
        }
    }

    pub(super) fn build_body(&self, tools: &[ToolSpec], opts: &TurnOpts) -> Value {
        build_body(&self.input, &self.session_id, tools, opts)
    }

    pub(super) fn parse_sse(&mut self, sse: &str) -> Result<TurnOutput> {
        parse_sse(&mut self.input, sse)
    }

    pub(super) fn identity_auth_headers(&self) -> Vec<(&'static str, String)> {
        identity_auth_headers(&self.session_id, &self.thread_id, &self.auth)
    }
}

/// Resolve auth from env: an explicit `OPENAI_API_KEY` selects the standard
/// OpenAI path; otherwise fall back to the Codex ChatGPT OAuth in
/// `~/.codex/auth.json` (loading + refreshing cooperatively with the Codex CLI).
pub(super) async fn resolve_auth(http: &reqwest::Client) -> Result<Auth> {
    if let Some(key) = super::session_var("OPENAI_API_KEY") {
        return Ok(Auth::ApiKey(key));
    }
    let auth = super::codex_auth::load_fresh(http)
        .await
        .context("no OPENAI_API_KEY and could not load/refresh Codex ChatGPT auth")?;
    Ok(Auth::ChatGpt {
        access_token: auth.access_token,
        account_id: auth.account_id,
    })
}

/// The HTTP `/responses` endpoint for the resolved auth: `{OPENAI_BASE_URL}/responses`
/// for an API key, or the ChatGPT backend (`OPENAI_RESPONSES_URL`) for OAuth.
pub(super) fn http_endpoint(auth: &Auth) -> String {
    match auth {
        Auth::ApiKey(_) => {
            let base = super::session_var("OPENAI_BASE_URL")
                .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
                .trim_end_matches('/')
                .to_string();
            format!("{base}/responses")
        }
        Auth::ChatGpt { .. } => super::session_var("OPENAI_RESPONSES_URL")
            .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex/responses".to_string()),
    }
}

/// The WebSocket `/responses` URL — the HTTP endpoint with the scheme swapped to
/// `wss`/`ws` (codex's `websocket_url_for_path`).
pub(super) fn ws_endpoint(auth: &Auth) -> String {
    let http = http_endpoint(auth);
    if let Some(rest) = http.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = http.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        http
    }
}

/// The codex-style identity + auth headers shared by every Responses request —
/// HTTP body POST and WS handshake alike. `session-id` is the stable per-session
/// id; `thread-id` is the current turn. `OpenAI-Beta: responses=experimental` is
/// intentionally absent (defunct in codex `main`). Returns `(name, value)` pairs
/// so each transport can apply them to its own request type; transport-specific
/// headers (content-type/accept on HTTP, the websockets beta on the WS
/// handshake) are added by the caller.
pub(super) fn identity_auth_headers(
    session_id: &str,
    thread_id: &str,
    auth: &Auth,
) -> Vec<(&'static str, String)> {
    let mut headers = vec![
        ("originator", originator()),
        ("user-agent", user_agent()),
        ("thread-id", thread_id.to_string()),
    ];
    if !session_id.is_empty() {
        headers.push(("session-id", session_id.to_string()));
    }
    match auth {
        Auth::ApiKey(k) => headers.push(("authorization", format!("Bearer {k}"))),
        Auth::ChatGpt {
            access_token,
            account_id,
        } => {
            headers.push(("authorization", format!("Bearer {access_token}")));
            headers.push(("chatgpt-account-id", account_id.clone()));
        }
    }
    headers
}

/// Build the Responses request body (pure; no I/O). The system split (stable
/// `instructions` + a trailing volatile `developer` item that is never persisted
/// into the buffer) is unit-testable. `input` is the conversation buffer;
/// `session_id` keys the prompt cache.
pub(super) fn build_body(
    input: &[Value],
    session_id: &str,
    tools: &[ToolSpec],
    opts: &TurnOpts,
) -> Value {
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
    // never persists into the buffer and can't disturb the cached prefix.
    let mut input = input.to_vec();
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
    // Stable cache key (codex uses the thread id): keeps the cached prefix
    // pinned to this session instead of relying on implicit server keying.
    if !session_id.is_empty() {
        body["prompt_cache_key"] = json!(session_id);
    }
    // `/fast` lever: forward the priority/flex tier when set (and not the
    // literal "default", which the backend rejects as a no-op).
    if let Some(tier) = service_tier_for_request(opts.service_tier.as_deref()) {
        body["service_tier"] = json!(tier);
    }
    // Reasoning: only for reasoning-capable models, and only when an effort
    // was requested. Bundle the summary (codex default `auto`) and request
    // encrypted reasoning so reasoning items can be replayed across turns
    // under `store:false` (see `parse_sse`).
    if let Some(e) = &opts.effort {
        if model_supports_reasoning(&opts.model) {
            let mut reasoning = json!({ "effort": normalize_effort(e) });
            if let Some(summary) = reasoning_summary() {
                reasoning["summary"] = json!(summary);
            }
            body["reasoning"] = reasoning;
            body["include"] = json!(["reasoning.encrypted_content"]);
        } else {
            tracing::warn!(
                model = %opts.model,
                "effort requested but model is not reasoning-capable; omitting reasoning"
            );
        }
    }
    body
}

/// Build the unary `responses/compact` request body (`CompactionInput`). The
/// backend applies retention server-side and returns the replacement `input[]`
/// (retained user/developer/system messages + one encrypted `compaction_summary`
/// item), so this is just the current history + tools + instructions — no
/// streaming, no `store`, no plaintext rendering. Codex-faithful minimal shape,
/// validated live (design/bro-harness/brodex-compaction.md §5).
pub(super) fn build_compaction_input(
    input: &[Value],
    tools: &[ToolSpec],
    opts: &TurnOpts,
) -> Value {
    let tool_defs: Vec<Value> = tools
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
    let instructions = opts
        .system
        .stable_text()
        .unwrap_or("You are a helpful coding assistant operating non-interactively.");
    json!({
        "model": opts.model,
        "input": input,
        "instructions": instructions,
        "tools": tool_defs,
        "parallel_tool_calls": false,
    })
}

/// Parse the SSE body: accumulate completed output items, append them to the
/// `input` buffer (so the next turn carries context), and normalize. Shared by
/// every Responses transport — the downstream event vocabulary is identical
/// across HTTP-SSE and WebSocket.
pub(super) fn parse_sse(input: &mut Vec<Value>, sse: &str) -> Result<TurnOutput> {
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
                    // Otherwise-silent path: the model stopped short (e.g.
                    // max_output_tokens, content filter). Surface the reason
                    // so a spurious-stop turn is diagnosable from the log.
                    tracing::warn!(
                        reason = %r["incomplete_details"]["reason"]
                            .as_str()
                            .unwrap_or("unknown"),
                        "responses turn incomplete; stopping short"
                    );
                }
            }
            "response.failed" | "error" => {
                let err = if ev["type"] == "response.failed" {
                    &ev["response"]["error"]
                } else {
                    &ev["error"]
                };
                let code = err["code"]
                    .as_str()
                    .or_else(|| ev["code"].as_str())
                    .unwrap_or("");
                let message = err["message"]
                    .as_str()
                    .or_else(|| ev["message"].as_str())
                    .unwrap_or(data);
                // A context-window rejection is recoverable: surface a typed
                // cause so the agent loop can compact + retry rather than fail
                // the turn. Flows through both the HTTP path and the WS path
                // (WsOutcome::Api), since both classify via this parser.
                if matches!(code, "context_length_exceeded" | "context_window_exceeded") {
                    return Err(anyhow::Error::new(super::ContextWindowExceeded(
                        classify_stream_error(code, message),
                    )));
                }
                anyhow::bail!(classify_stream_error(code, message));
            }
            _ => {}
        }
    }

    // Echo the model's output items back into the buffer for continuity.
    // Reasoning items need care under `store:false` (required by the ChatGPT
    // backend): a reasoning item replayed *by reference* (`rs_…` with no
    // payload) 404s ("Item with id … not found") because it isn't persisted
    // server-side. But because we request `include:["reasoning.encrypted_content"]`,
    // reasoning items come back carrying `encrypted_content` — self-contained
    // and safe to replay, preserving cross-turn reasoning continuity. So:
    // keep reasoning items that carry `encrypted_content`; drop the rest;
    // keep every non-reasoning item.
    input.extend(
        output_items
            .iter()
            .filter(|item| {
                item["type"].as_str() != Some("reasoning")
                    || item
                        .get("encrypted_content")
                        .and_then(Value::as_str)
                        .is_some()
            })
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
                    tool_calls.push(ToolCall {
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

/// Largest split `≤ limit` whose kept tail `[split..]` is a valid standalone
/// Responses input — no `function_call_output` orphaned from the
/// `function_call` (matched by `call_id`) it answers. `None` if none exists.
pub(super) fn responses_split(input: &[Value], limit: usize) -> Option<usize> {
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
pub(super) fn render_responses_transcript(items: &[Value], tool_cap: usize) -> String {
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
                super::truncate(it["output"].as_str().unwrap_or(""), tool_cap)
            )),
            _ => {}
        }
    }
    s
}

/// Map an effort token onto codex's `ReasoningEffort` range
/// (`none/minimal/low/medium/high/xhigh`). `max` stays conservative at `high`
/// (universally supported); callers wanting `xhigh` (newer, model-specific)
/// pass it explicitly.
pub(super) fn normalize_effort(e: &str) -> &'static str {
    match e.trim().to_ascii_lowercase().as_str() {
        "none" => "none",
        "minimal" | "min" => "minimal",
        "low" => "low",
        "medium" | "med" => "medium",
        "high" => "high",
        "xhigh" | "x-high" | "extra-high" => "xhigh",
        "max" => "high",
        _ => "medium",
    }
}

/// Fresh random id for the `session-id`/`thread-id` headers.
pub(super) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Request originator — codex's first-party value by default so the ChatGPT
/// backend routes/accounts the request as it expects. Overridable to match
/// codex's own `CODEX_INTERNAL_ORIGINATOR_OVERRIDE`, or `BRO_HARNESS_ORIGINATOR`.
pub(super) fn originator() -> String {
    std::env::var("CODEX_INTERNAL_ORIGINATOR_OVERRIDE")
        .or_else(|_| std::env::var("BRO_HARNESS_ORIGINATOR"))
        .unwrap_or_else(|_| "codex_cli_rs".to_string())
}

/// Descriptive `User-Agent` in codex's shape (`<originator>/<ver> (<os>; <arch>)`),
/// fully overridable via `BRO_HARNESS_USER_AGENT`.
pub(super) fn user_agent() -> String {
    std::env::var("BRO_HARNESS_USER_AGENT").unwrap_or_else(|_| {
        format!(
            "{}/{} ({}; {})",
            originator(),
            env!("CARGO_PKG_VERSION"),
            std::env::consts::OS,
            std::env::consts::ARCH,
        )
    })
}

/// True unless the model is a known non-reasoning family. Codex gates reasoning
/// on a per-model capability flag from its catalog; lacking that catalog we
/// fail open (unknown ⇒ reasoning allowed, since the ChatGPT backend only ever
/// serves reasoning models) but suppress the obvious GPT-3/4 families so an
/// effort value can't 400 a non-reasoning model.
pub(super) fn model_supports_reasoning(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    const NON_REASONING_PREFIXES: &[&str] = &["gpt-4", "gpt-3", "chatgpt-4"];
    !NON_REASONING_PREFIXES.iter().any(|p| m.starts_with(p))
}

/// Reasoning summary mode (codex default `auto`). `BRO_HARNESS_REASONING_SUMMARY`
/// overrides; `none`/`off`/empty omits the field.
pub(super) fn reasoning_summary() -> Option<String> {
    match std::env::var("BRO_HARNESS_REASONING_SUMMARY") {
        Ok(v) if matches!(v.trim().to_ascii_lowercase().as_str(), "none" | "off" | "") => None,
        Ok(v) => Some(v.trim().to_string()),
        Err(_) => Some("auto".to_string()),
    }
}

/// Normalize a requested service tier: forward it unless it's empty or the
/// literal `"default"` (which the backend rejects as a no-op). Codex's
/// `service_tier_for_request` does the same drop.
pub(super) fn service_tier_for_request(tier: Option<&str>) -> Option<String> {
    let t = tier?.trim();
    if t.is_empty() || t.eq_ignore_ascii_case("default") {
        return None;
    }
    Some(t.to_string())
}

/// Classify a Responses stream error (`response.failed` / `error`) into a clear,
/// actionable message. Mirrors codex's error-code mapping
/// (`codex-api/src/sse/responses.rs`).
pub(super) fn classify_stream_error(code: &str, message: &str) -> String {
    match code {
        "context_length_exceeded" | "context_window_exceeded" => {
            format!("context window exceeded ({message}); compact the conversation and retry")
        }
        "insufficient_quota" | "usage_not_included" => {
            format!("quota/usage limit [{code}]: {message}")
        }
        "server_is_overloaded" | "slow_down" => format!("server overloaded [{code}]: {message}"),
        "" => format!("responses stream error: {message}"),
        _ => format!("responses stream error [{code}]: {message}"),
    }
}

/// Classify a non-2xx HTTP response, surfacing any error code from the body
/// envelope so failures are diagnosable from the log.
pub(super) fn classify_http_error(status: reqwest::StatusCode, body: &str) -> String {
    let code = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["code"]
                .as_str()
                .or_else(|| v["error"]["type"].as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if code.is_empty() {
        format!("openai responses {status}: {body}")
    } else {
        format!("openai responses {status} [{code}]: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::SystemPrompt;

    fn state() -> ResponsesState {
        let mut s = ResponsesState::new(Auth::ApiKey("k".into()));
        s.session_id = "sess-1".into();
        s.input = vec![json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}],
        })];
        s
    }
    fn opts(system: SystemPrompt) -> TurnOpts {
        TurnOpts {
            model: "gpt-5-codex".into(),
            max_tokens: 16,
            system,
            effort: None,
            web_search: false,
            service_tier: None,
        }
    }

    #[test]
    fn stable_is_instructions_volatile_is_trailing_developer_item() {
        let body = state().build_body(
            &[],
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("MANIFEST".into()),
            }),
        );
        assert_eq!(body["instructions"], "BASE");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[1]["role"], "developer");
        assert_eq!(input[1]["content"][0]["text"], "MANIFEST");
        // Volatile never persisted into the buffer.
        assert_eq!(state().input.len(), 1);
    }

    #[test]
    fn empty_stable_falls_back_to_nonempty_instructions() {
        let body = state().build_body(
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
    fn modern_body_carries_cache_key_service_tier_and_reasoning() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.effort = Some("medium".into());
        o.service_tier = Some("priority".into());
        let body = state().build_body(&[], &o);
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["prompt_cache_key"], "sess-1");
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["reasoning"]["effort"], "medium");
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
    }

    #[test]
    fn default_service_tier_is_dropped() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.service_tier = Some("default".into());
        let body = state().build_body(&[], &o);
        assert!(body.get("service_tier").is_none());
    }

    #[test]
    fn reasoning_omitted_for_non_reasoning_model() {
        let mut o = opts(SystemPrompt {
            stable: Some("BASE".into()),
            volatile: None,
        });
        o.model = "gpt-4o".into();
        o.effort = Some("high".into());
        let body = state().build_body(&[], &o);
        assert!(body.get("reasoning").is_none());
        assert!(body.get("include").is_none());
    }

    #[test]
    fn reasoning_item_replayed_only_with_encrypted_content() {
        let mut s = state();
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"ENC\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"bare\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        s.parse_sse(sse).unwrap();
        let reasoning: Vec<_> = s
            .input
            .iter()
            .filter(|i| i["type"] == "reasoning")
            .collect();
        assert_eq!(reasoning.len(), 1);
        assert_eq!(reasoning[0]["encrypted_content"], "ENC");
    }

    #[test]
    fn parse_sse_surfaces_reasoning_as_thinking() {
        let sse = concat!(
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"pondering\"}]}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"answer\"}]}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":5,\"output_tokens\":3}}}\n",
        );
        let out = state().parse_sse(sse).unwrap();
        assert_eq!(out.text, "answer");
        assert_eq!(out.thinking, "pondering");
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
    fn pure_helpers() {
        assert_eq!(normalize_effort("minimal"), "minimal");
        assert_eq!(normalize_effort("max"), "high");
        assert_eq!(normalize_effort("xhigh"), "xhigh");
        assert_eq!(normalize_effort("bogus"), "medium");
        assert!(model_supports_reasoning("gpt-5-codex"));
        assert!(model_supports_reasoning("o3"));
        assert!(!model_supports_reasoning("gpt-4o"));
        assert!(!model_supports_reasoning("gpt-4.1"));
        assert_eq!(
            service_tier_for_request(Some("priority")).as_deref(),
            Some("priority")
        );
        assert_eq!(service_tier_for_request(Some("default")), None);
        assert_eq!(service_tier_for_request(Some("")), None);
        assert_eq!(service_tier_for_request(None), None);
    }

    #[test]
    fn identity_auth_headers_shape() {
        let h = identity_auth_headers(
            "sess-1",
            "thread-1",
            &Auth::ChatGpt {
                access_token: "tok".into(),
                account_id: "acct".into(),
            },
        );
        let get = |k: &str| h.iter().find(|(n, _)| *n == k).map(|(_, v)| v.as_str());
        assert_eq!(get("session-id"), Some("sess-1"));
        assert_eq!(get("thread-id"), Some("thread-1"));
        assert_eq!(get("authorization"), Some("Bearer tok"));
        assert_eq!(get("chatgpt-account-id"), Some("acct"));
        assert_eq!(get("originator"), Some("codex_cli_rs"));
        // No defunct beta header; api-key path carries no account id.
        assert!(h.iter().all(|(n, _)| *n != "OpenAI-Beta"));
        let api = identity_auth_headers("", "t", &Auth::ApiKey("k".into()));
        assert!(api.iter().all(|(n, _)| *n != "session-id")); // empty session id omitted
        assert!(api.iter().all(|(n, _)| *n != "chatgpt-account-id"));
    }

    #[test]
    fn classify_stream_error_names_codes() {
        assert!(
            classify_stream_error("context_length_exceeded", "too big").contains("context window")
        );
        assert!(classify_stream_error("server_is_overloaded", "busy").contains("overloaded"));
        assert!(classify_stream_error("", "boom").contains("boom"));
    }

    #[test]
    fn build_compaction_input_is_codex_faithful() {
        let input = vec![json!({
            "type": "message", "role": "user",
            "content": [{"type": "input_text", "text": "hi"}]
        })];
        let tools = vec![ToolSpec {
            name: "do_thing".into(),
            description: "does a thing".into(),
            schema: json!({"type": "object"}),
        }];
        let body = build_compaction_input(
            &input,
            &tools,
            &opts(SystemPrompt {
                stable: Some("BASE".into()),
                volatile: Some("VOL".into()),
            }),
        );
        assert_eq!(body["model"], "gpt-5-codex");
        // Stable instructions only; the volatile developer item is NOT appended
        // (unlike a normal turn) — the server compacts the literal history.
        assert_eq!(body["instructions"], "BASE");
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["parallel_tool_calls"], false);
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["name"], "do_thing");
        // Unary endpoint: no stream / store fields.
        assert!(body.get("stream").is_none());
        assert!(body.get("store").is_none());
    }

    #[test]
    fn parse_sse_context_window_error_is_typed_recoverable() {
        // A context-window rejection must surface as the typed, recoverable
        // ContextWindowExceeded cause so the agent loop can compact + retry.
        let sse = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"context_window_exceeded\",\"message\":\"too big\"}}}\n";
        let err = match state().parse_sse(sse) {
            Ok(_) => panic!("must error on response.failed"),
            Err(e) => e,
        };
        assert!(
            crate::transport::is_context_window_exceeded(&err),
            "context_window_exceeded should be the typed recoverable error, got: {err:#}"
        );

        // Any other failure code stays a plain (non-recoverable) error.
        let other = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"server_is_overloaded\",\"message\":\"busy\"}}}\n";
        let err = match state().parse_sse(other) {
            Ok(_) => panic!("must error on response.failed"),
            Err(e) => e,
        };
        assert!(
            !crate::transport::is_context_window_exceeded(&err),
            "overload must not be classified as context-window: {err:#}"
        );
    }

    /// LIVE PROBE (ignored, double-gated). Answers the open questions in
    /// design/bro-harness/brodex-compaction.md §5 against the real ChatGPT
    /// backend: does our account honor (a) the unary `responses/compact`
    /// endpoint and (b) the streaming `compaction_trigger` item? Self-validating:
    /// a normal `/responses` call first proves model+auth, so a compaction
    /// failure is attributable to the compaction surface, not setup.
    ///
    /// Run with:
    ///   BRO_HARNESS_LIVE_PROBE=1 [BRO_HARNESS_PROBE_MODEL=gpt-5.1-codex] \
    ///     cargo test -p bro-harness --bins probe_responses_compaction \
    ///     -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "live backend probe; set BRO_HARNESS_LIVE_PROBE=1"]
    async fn probe_responses_compaction() {
        if std::env::var("BRO_HARNESS_LIVE_PROBE").is_err() {
            eprintln!("skip: set BRO_HARNESS_LIVE_PROBE=1 to run the live probe");
            return;
        }
        let model = std::env::var("BRO_HARNESS_PROBE_MODEL")
            .unwrap_or_else(|_| "gpt-5.1-codex".to_string());
        let http = reqwest::Client::new();
        let auth = resolve_auth(&http)
            .await
            .expect("resolve auth (refreshes token)");
        let endpoint = http_endpoint(&auth);
        let compact_url = format!("{endpoint}/compact");
        let session_id = new_id();
        let thread_id = new_id();
        let headers = identity_auth_headers(&session_id, &thread_id, &auth);
        eprintln!("[probe] model={model} endpoint={endpoint}");

        let convo = json!([
            {"type":"message","role":"user","content":[{"type":"input_text",
                "text":"Step 1: remember the magic token BANANA-7. Acknowledge briefly."}]},
            {"type":"message","role":"assistant","content":[{"type":"output_text",
                "text":"Acknowledged — magic token BANANA-7 noted."}]},
            {"type":"message","role":"user","content":[{"type":"input_text",
                "text":"Step 2: now compute 17 * 3 and explain in one line."}]}
        ]);
        let send = |url: String, body: serde_json::Value, sse: bool| {
            let http = http.clone();
            let headers = headers.clone();
            async move {
                let mut rb = http.post(&url).header("content-type", "application/json");
                rb = rb.header(
                    "accept",
                    if sse {
                        "text/event-stream"
                    } else {
                        "application/json"
                    },
                );
                for (n, v) in &headers {
                    rb = rb.header(*n, v);
                }
                let resp = rb.json(&body).send().await.expect("send");
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                (status, text)
            }
        };
        let head = |s: &str, n: usize| s.chars().take(n).collect::<String>();

        // 1. Normal /responses — proves model + auth + headers.
        let (st, body) = send(
            endpoint.clone(),
            json!({"model": model, "input": convo, "instructions": "You are a helpful assistant.",
                   "stream": true, "store": false, "tool_choice": "auto", "parallel_tool_calls": false}),
            true,
        )
        .await;
        eprintln!(
            "\n[probe 1] NORMAL /responses -> {st}\n{}",
            head(&body, 800)
        );

        // 2. Unary /responses/compact — CompactionInput shape.
        let (st, body2) = send(
            compact_url.clone(),
            json!({"model": model, "input": convo, "instructions": "You are a helpful assistant.",
                   "tools": [], "parallel_tool_calls": false}),
            false,
        )
        .await;
        eprintln!(
            "\n[probe 2] UNARY /responses/compact -> {st}\n{}",
            head(&body2, 1500)
        );

        // 3. Streaming compaction_trigger on /responses.
        let mut trig = convo.as_array().unwrap().clone();
        trig.push(json!({"type": "compaction_trigger"}));
        let (st, body3) = send(
            endpoint.clone(),
            json!({"model": model, "input": trig, "instructions": "You are a helpful assistant.",
                   "stream": true, "store": false}),
            true,
        )
        .await;
        eprintln!(
            "\n[probe 3] STREAM compaction_trigger -> {st}\n{}",
            head(&body3, 1200)
        );

        // 4. REPLAY: feed the unary-compacted output (incl. the encrypted
        // compaction_summary) back as input + a fresh user turn. Proves the
        // canonical loop: does the backend accept the encrypted summary on
        // replay under store:false, and does the model answer from it?
        let parsed: serde_json::Value = serde_json::from_str(&body2).unwrap_or_else(|_| json!({}));
        match parsed["output"].as_array() {
            Some(out) if !out.is_empty() => {
                let kinds: Vec<String> = out
                    .iter()
                    .map(|i| i["type"].as_str().unwrap_or("?").to_string())
                    .collect();
                eprintln!("\n[probe 4] compacted output item types: {kinds:?}");
                let mut replay = out.clone();
                replay.push(json!({"type":"message","role":"user","content":[{"type":"input_text",
                    "text":"Using only the prior context, answer in one short line: what magic token did I give you, and what is 17*3?"}]}));
                let (st, body4) = send(
                    endpoint.clone(),
                    json!({"model": model, "input": replay, "instructions": "You are a helpful assistant.",
                           "stream": true, "store": false}),
                    true,
                )
                .await;
                eprintln!("[probe 4] REPLAY compacted history -> {st}");
                // Surface only the output_text deltas so we can see the answer.
                let answer: String = body4
                    .lines()
                    .filter_map(|l| l.strip_prefix("data:"))
                    .filter_map(|d| serde_json::from_str::<serde_json::Value>(d.trim()).ok())
                    .filter(|e| e["type"] == "response.output_text.delta")
                    .filter_map(|e| e["delta"].as_str().map(str::to_string))
                    .collect();
                eprintln!("[probe 4] model answer: {}", head(&answer, 400));
                eprintln!(
                    "[probe 4] mentions BANANA-7: {}",
                    answer.contains("BANANA-7")
                );
                eprintln!("[probe 4] mentions 51: {}", answer.contains("51"));
            }
            _ => eprintln!("\n[probe 4] skipped: no output array in unary compact body"),
        }
    }
}

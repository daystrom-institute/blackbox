//! Transport-agnostic building blocks shared by the OpenAI Responses transports
//! (HTTP-SSE today; a WebSocket transport next — see
//! `design/bro-harness/brodex-websocket-transport.md`).
//!
//! Everything here is independent of the wire/connection mechanism: the auth
//! enum + resolution, the request-body builder, the SSE/event reconstruction,
//! the identity+auth header *values* (each transport applies them to its own
//! request type — reqwest builder vs WS handshake), and the small pure mappers.
//! Each transport owns only its connection lifecycle and framing.

use super::{StopReason, ToolCall, ToolSpec, TurnOpts, TurnOutput, Usage};
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

/// Resolve auth from env: an explicit `OPENAI_API_KEY` selects the standard
/// OpenAI path; otherwise fall back to the Codex ChatGPT OAuth in
/// `~/.codex/auth.json` (loading + refreshing cooperatively with the Codex CLI).
pub(super) async fn resolve_auth(http: &reqwest::Client) -> Result<Auth> {
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
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
            let base = std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string())
                .trim_end_matches('/')
                .to_string();
            format!("{base}/responses")
        }
        Auth::ChatGpt { .. } => std::env::var("OPENAI_RESPONSES_URL")
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex/responses".to_string()),
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
                    || item.get("encrypted_content").and_then(Value::as_str).is_some()
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
pub(super) fn render_responses_transcript(items: &[Value]) -> String {
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
}

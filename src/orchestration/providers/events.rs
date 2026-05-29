use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::Provider;

/// Mutable state that event parsing updates on a Task.
pub struct EventSink {
    pub last_assistant_message: Option<String>,
    pub usage: Option<Usage>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub session_id: Option<String>,
}

/// Normalized per-task token usage.
///
/// Token-counter semantics differ wildly across provider wire formats, so we
/// normalize every provider into one convention here:
///
/// * `input_tokens` is **fresh** (cache-exclusive) prompt input — the tokens
///   the model actually had to process anew this session. This is what should
///   drive load/burn signals; reporting cache-inclusive input as the headline
///   overstates real work (a long codex session can read millions of cached
///   tokens it never reprocessed).
/// * `cached_input_tokens` is the cache-read portion (tokens served from the
///   provider's prompt cache).
/// * `cache_creation_input_tokens` is the cache-write portion (tokens written
///   into the cache this turn; Anthropic bills these separately).
///
/// Total prompt input (what some providers, e.g. codex/OpenAI, report as their
/// headline `input_tokens`) is therefore
/// `input_tokens + cached_input_tokens + cache_creation_input_tokens`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Cache-read input tokens (served from prompt cache, not reprocessed).
    #[serde(default)]
    pub cached_input_tokens: u64,
    /// Cache-creation input tokens (written into the prompt cache this turn).
    #[serde(default)]
    pub cache_creation_input_tokens: u64,
}

impl Usage {
    /// Total prompt input including cache reads + cache creation. This is the
    /// cache-inclusive figure codex/OpenAI report as their headline input.
    pub fn total_input_tokens(&self) -> u64 {
        self.input_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_creation_input_tokens)
    }
}

impl Provider {
    /// Parse a streaming JSON event and update the sink.
    pub fn parse_event(&self, evt: &Value, sink: &mut EventSink) {
        match self {
            Provider::Claude | Provider::Glm | Provider::Deepseek | Provider::Brodex => {
                parse_claude_event(evt, sink)
            }
            Provider::Inception => parse_opencode_event(evt, sink),
            Provider::Codex => parse_codex_event(evt, sink),
            Provider::Copilot => parse_copilot_event(evt, sink),
            Provider::Vibe => parse_vibe_event(evt, sink),
            Provider::Gemini => parse_gemini_event(evt, sink),
            Provider::Workflow => {}
        }
    }

    /// For non-streaming providers, parse the full stdout after process exit.
    pub fn parse_bulk_output(&self, raw: &str, sink: &mut EventSink) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            self.parse_event(&parsed, sink);
        } else {
            sink.last_assistant_message = Some(raw.trim().to_string());
        }
    }

    pub fn build_export_args(&self, session_id: &str) -> Option<Vec<String>> {
        match self {
            Provider::Inception => Some(vec!["export".into(), session_id.into()]),
            _ => None,
        }
    }
}

fn append_block_separator(buf: &mut Option<String>) {
    if let Some(existing) = buf.as_mut()
        && !existing.is_empty()
    {
        existing.push_str("\n\n");
    }
}

fn parse_claude_event(evt: &Value, sink: &mut EventSink) {
    if evt["type"].as_str() == Some("system") {
        let subtype = evt["subtype"].as_str();
        if matches!(subtype, Some("hook_started") | Some("hook_response")) {
            return;
        }
    }
    if let Some(session_id) = evt["session_id"]
        .as_str()
        .or_else(|| evt["sessionId"].as_str())
        .or_else(|| evt["message"]["session_id"].as_str())
        .or_else(|| evt["message"]["sessionId"].as_str())
    {
        sink.session_id = Some(session_id.to_string());
    }

    if evt["type"].as_str() == Some("stream_event") {
        let inner_ty = evt["event"]["type"].as_str().unwrap_or("");
        match inner_ty {
            "content_block_start"
                if evt["event"]["content_block"]["type"].as_str() == Some("text") =>
            {
                append_block_separator(&mut sink.last_assistant_message);
            }
            "content_block_delta"
                if evt["event"]["delta"]["type"].as_str() == Some("text_delta") =>
            {
                if let Some(chunk) = evt["event"]["delta"]["text"].as_str() {
                    let buf = sink.last_assistant_message.get_or_insert_with(String::new);
                    buf.push_str(chunk);
                }
            }
            _ => {}
        }
    }
    if evt["type"].as_str() == Some("assistant") {
        let streaming_captured = sink
            .last_assistant_message
            .as_deref()
            .is_some_and(|m| !m.is_empty());
        if !streaming_captured && let Some(content) = evt["message"]["content"].as_array() {
            for block in content {
                if block["type"].as_str() == Some("text")
                    && let Some(text) = block["text"].as_str()
                {
                    if text.is_empty() {
                        continue;
                    }
                    append_block_separator(&mut sink.last_assistant_message);
                    let buf = sink.last_assistant_message.get_or_insert_with(String::new);
                    buf.push_str(text);
                }
            }
        }
    }
    if evt["type"].as_str() == Some("result") {
        if let Some(result) = evt["result"].as_str() {
            let already_have_streamed_text = sink
                .last_assistant_message
                .as_deref()
                .is_some_and(|m| !m.is_empty());
            if !result.is_empty() && !already_have_streamed_text {
                sink.last_assistant_message = Some(result.to_string());
            }
        }
        if let Some(usage) = evt["usage"].as_object() {
            // Anthropic's `input_tokens` is already fresh (cache-exclusive);
            // cache reads/writes are reported separately. This matches our
            // normalized convention directly. Harness providers (glm/deepseek/
            // brodex) emit the same Anthropic-native shape via bro-harness.
            sink.usage = Some(Usage {
                input_tokens: usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                output_tokens: usage
                    .get("output_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cached_input_tokens: usage
                    .get("cache_read_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                cache_creation_input_tokens: usage
                    .get("cache_creation_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
            });
        }
        sink.cost_usd = evt["total_cost_usd"].as_f64();
        sink.num_turns = evt["num_turns"].as_u64();
    }
}

fn parse_opencode_event(evt: &Value, sink: &mut EventSink) {
    if let Some(session_id) = evt["sessionID"].as_str() {
        sink.session_id = Some(session_id.to_string());
    }
    if evt["type"].as_str() == Some("step_start") {
        sink.last_assistant_message = None;
    } else if evt["type"].as_str() == Some("text")
        && let Some(text) = evt["part"]["text"].as_str()
        && !text.is_empty()
    {
        sink.last_assistant_message = Some(text.to_string());
    }
}

pub fn parse_opencode_export(raw: &str, sink: &mut EventSink) {
    let Some(json_start) = raw.find('{') else {
        return;
    };
    let Ok(export) = serde_json::from_str::<Value>(&raw[json_start..]) else {
        return;
    };

    if sink.session_id.is_none() {
        sink.session_id = export["info"]["id"].as_str().map(str::to_string);
    }

    let Some(messages) = export["messages"].as_array() else {
        return;
    };

    let assistant_messages: Vec<&Value> = messages
        .iter()
        .filter(|msg| msg["info"]["role"].as_str() == Some("assistant"))
        .collect();

    sink.num_turns = Some(assistant_messages.len() as u64);

    let Some(last_assistant) = assistant_messages.last() else {
        return;
    };

    let text_parts: Vec<&str> = last_assistant["parts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|part| part["type"].as_str() == Some("text"))
        .filter_map(|part| part["text"].as_str())
        .collect();
    if !text_parts.is_empty() {
        sink.last_assistant_message = Some(text_parts.join("\n"));
    }

    let tokens = &last_assistant["info"]["tokens"];
    let input_tokens = tokens["input"].as_u64();
    let output_tokens = tokens["output"].as_u64();
    if let (Some(input_tokens), Some(output_tokens)) = (input_tokens, output_tokens) {
        // OpenCode reports cache reads/writes under `tokens.cache.{read,write}`
        // and keeps `tokens.input` fresh (cache-exclusive), so it already
        // matches our convention.
        sink.usage = Some(Usage {
            input_tokens,
            output_tokens,
            cached_input_tokens: tokens["cache"]["read"].as_u64().unwrap_or(0),
            cache_creation_input_tokens: tokens["cache"]["write"].as_u64().unwrap_or(0),
        });
    }

    sink.cost_usd = last_assistant["info"]["cost"].as_f64();
}

fn parse_codex_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        "item.completed" => {
            if let Some(item) = evt.get("item")
                && item["type"].as_str() == Some("agent_message")
                && let Some(text) = item["text"].as_str()
            {
                sink.last_assistant_message = Some(text.to_string());
            }
        }
        "turn.completed" => {
            if let Some(usage) = evt["usage"].as_object() {
                // Codex reports cumulative-per-session, cache-INCLUSIVE input:
                // `input_tokens` already contains `cached_input_tokens`. Split
                // it so our `input_tokens` stays fresh (cache-exclusive) like
                // every other provider; otherwise a cache-heavy session
                // overstates real input load by orders of magnitude.
                let total_input = usage
                    .get("input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                let cached = usage
                    .get("cached_input_tokens")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                sink.usage = Some(Usage {
                    input_tokens: total_input.saturating_sub(cached),
                    output_tokens: usage
                        .get("output_tokens")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0),
                    cached_input_tokens: cached,
                    // Codex does not separately report cache-creation tokens.
                    cache_creation_input_tokens: 0,
                });
            }
        }
        "thread.started" => {
            if let Some(tid) = evt["thread_id"].as_str() {
                sink.session_id = Some(tid.to_string());
            }
        }
        _ => {}
    }
}

fn parse_copilot_event(evt: &Value, sink: &mut EventSink) {
    let msg_type = evt["type"].as_str().unwrap_or("");
    match msg_type {
        "assistant.message" => {
            if let Some(data) = evt.get("data")
                && let Some(content) = data["content"].as_str()
            {
                sink.last_assistant_message = Some(content.to_string());
            }
        }
        "session.task_complete" => {
            if let Some(data) = evt.get("data")
                && let Some(summary) = data["summary"].as_str()
            {
                sink.last_assistant_message = Some(summary.to_string());
            }
        }
        "result" => {
            if let Some(sid) = evt["sessionId"].as_str() {
                sink.session_id = Some(sid.to_string());
            }
            if let Some(usage) = evt["usage"].as_object() {
                // Copilot bills in premium requests, not tokens, and usually
                // omits token counters entirely. Read them when present
                // (camelCase, matching `premiumRequests`, with snake_case
                // fallback) instead of blindly hardcoding 0 — a silent 0
                // masks real usage whenever the CLI does surface tokens.
                let pick = |camel: &str, snake: &str| -> u64 {
                    usage
                        .get(camel)
                        .or_else(|| usage.get(snake))
                        .and_then(|v| v.as_u64())
                        .unwrap_or(0)
                };
                sink.usage = Some(Usage {
                    input_tokens: pick("inputTokens", "input_tokens"),
                    output_tokens: pick("outputTokens", "output_tokens"),
                    cached_input_tokens: pick("cachedInputTokens", "cached_input_tokens"),
                    cache_creation_input_tokens: pick(
                        "cacheCreationInputTokens",
                        "cache_creation_input_tokens",
                    ),
                });
                sink.num_turns = usage.get("premiumRequests").and_then(|v| v.as_u64());
            }
        }
        _ => {}
    }
}

fn parse_vibe_event(evt: &Value, sink: &mut EventSink) {
    if let Some(arr) = evt.as_array() {
        for msg in arr.iter().rev() {
            if msg["role"].as_str() == Some("assistant")
                && let Some(content) = msg["content"].as_str()
            {
                sink.last_assistant_message = Some(content.trim().to_string());
                break;
            }
        }
    }
}

fn parse_gemini_event(evt: &Value, sink: &mut EventSink) {
    if let Some(response) = evt["response"].as_str() {
        sink.last_assistant_message = Some(response.to_string());
    }
    if let Some(session_id) = evt["session_id"].as_str() {
        sink.session_id = Some(session_id.to_string());
    }
    if let Some(stats) = evt.get("stats")
        && let Some(models) = stats.get("models").and_then(|m| m.as_object())
        && let Some(first_model) = models.values().next()
        && let Some(tokens) = first_model.get("tokens")
    {
        // Gemini's `input` mirrors `promptTokenCount`, which is cache-INCLUSIVE
        // (`cached` ⊆ `input`). Subtract the cached subset so `input_tokens`
        // stays fresh. When `cached` is absent this is a no-op.
        let total_input = tokens["input"].as_u64().unwrap_or(0);
        let cached = tokens["cached"].as_u64().unwrap_or(0);
        sink.usage = Some(Usage {
            input_tokens: total_input.saturating_sub(cached),
            output_tokens: tokens["candidates"].as_u64().unwrap_or(0),
            cached_input_tokens: cached,
            cache_creation_input_tokens: 0,
        });
    }
}

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
    pub interrupted: bool,
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

/// A provider telling us, in its own words, that a lane is unavailable — a
/// rate-limit or overload. Detected only from explicit signals (an HTTP-ish
/// status code or the provider's own error text), never inferred from usage.
/// Feeds an allocator cooldown so dispatch steers off the disrupted lane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Disruption {
    RateLimited,
    Overloaded,
}

/// Map a dedicated error string to a disruption. Conservative — only explicit
/// provider rate-limit/overload phrasing, applied to error fields only.
fn disruption_from_error_text(s: &str) -> Option<Disruption> {
    let l = s.to_ascii_lowercase();
    if l.contains("rate_limit")
        || l.contains("rate limit")
        || l.contains("too many requests")
        || l.contains("resource_exhausted")
    {
        Some(Disruption::RateLimited)
    } else if l.contains("overloaded") {
        Some(Disruption::Overloaded)
    } else {
        None
    }
}

/// Streaming-event parsing for a provider (daemon-side: updates an
/// [`EventSink`] from the provider's wire envelope). Part of the provider
/// dispatch surface — see [`super::dispatch_prelude`].
pub trait ProviderEvents {
    /// Detect a rate-limit / overload disruption from a single streaming event.
    fn detect_disruption(&self, evt: &Value) -> Option<Disruption>;
    /// Parse a streaming JSON event and update the sink.
    fn parse_event(&self, evt: &Value, sink: &mut EventSink);
    /// For non-streaming providers, parse the full stdout after process exit.
    fn parse_bulk_output(&self, raw: &str, sink: &mut EventSink);
    /// Build provider-native transcript export args, if any.
    #[allow(dead_code)]
    fn build_export_args(&self, session_id: &str) -> Option<Vec<String>>;
}

impl ProviderEvents for Provider {
    /// Detect a rate-limit / overload disruption from a single streaming event.
    /// Structured `apiErrorStatus` (429/529) first — surfaced by the Claude CLI
    /// and the bro-harness Anthropic envelope (GLM/DeepSeek/MiniMax/Brodex) — then the
    /// provider's explicit error text on error records only.
    fn detect_disruption(&self, evt: &Value) -> Option<Disruption> {
        let status = evt["apiErrorStatus"]
            .as_u64()
            .or_else(|| evt["message"]["apiErrorStatus"].as_u64());
        match status {
            Some(429) => return Some(Disruption::RateLimited),
            Some(529) => return Some(Disruption::Overloaded),
            _ => {}
        }
        // Error text — only on dedicated error fields, not normal content.
        let error_texts = [
            evt["error"].as_str(),
            evt["error"]["message"].as_str(),
            if evt["is_error"].as_bool() == Some(true) {
                evt["result"].as_str()
            } else {
                None
            },
        ];
        error_texts
            .into_iter()
            .flatten()
            .find_map(disruption_from_error_text)
    }

    /// Parse a streaming JSON event and update the sink.
    fn parse_event(&self, evt: &Value, sink: &mut EventSink) {
        match self {
            Provider::Glm
            | Provider::Deepseek
            | Provider::Minimax
            | Provider::Kimi
            | Provider::Brodex
            | Provider::VibeBh => parse_claude_event(evt, sink),
            Provider::Workflow => {}
        }
    }

    /// For non-streaming providers, parse the full stdout after process exit.
    fn parse_bulk_output(&self, raw: &str, sink: &mut EventSink) {
        if let Ok(parsed) = serde_json::from_str::<Value>(raw) {
            self.parse_event(&parsed, sink);
        } else {
            sink.last_assistant_message = Some(raw.trim().to_string());
        }
    }

    fn build_export_args(&self, session_id: &str) -> Option<Vec<String>> {
        let _ = session_id;
        None
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
        sink.interrupted = result_event_interrupted(evt);
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
            // minimax/brodex) emit the same Anthropic-native shape via
            // bro-harness.
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

pub fn result_event_interrupted(evt: &Value) -> bool {
    evt["type"].as_str() == Some("result")
        && (evt["subtype"].as_str() == Some("interrupted")
            || evt["interrupted"].as_bool() == Some(true))
}

#[cfg(test)]
mod disruption_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_structured_api_error_status() {
        let rl = json!({"type": "assistant", "apiErrorStatus": 429, "isApiErrorMessage": true});
        assert_eq!(
            Provider::Glm.detect_disruption(&rl),
            Some(Disruption::RateLimited)
        );
        let ov = json!({"apiErrorStatus": 529});
        assert_eq!(
            Provider::Glm.detect_disruption(&ov),
            Some(Disruption::Overloaded)
        );
        // harness providers share the Claude envelope.
        assert_eq!(
            Provider::Glm.detect_disruption(&rl),
            Some(Disruption::RateLimited)
        );
    }

    #[test]
    fn detects_explicit_error_text_only_on_error_fields() {
        let err = json!({"type": "result", "is_error": true, "result": "Error: rate_limit_error — too many requests"});
        assert_eq!(
            Provider::Brodex.detect_disruption(&err),
            Some(Disruption::RateLimited)
        );
        let ov = json!({"error": {"message": "the model is overloaded, please retry"}});
        assert_eq!(
            Provider::Glm.detect_disruption(&ov),
            Some(Disruption::Overloaded)
        );
    }

    #[test]
    fn does_not_fire_on_normal_content_or_unrelated_status() {
        // The word "quota" in normal assistant text must NOT trip a disruption.
        let normal = json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "Your quota for the project looks fine."}]}});
        assert_eq!(Provider::Glm.detect_disruption(&normal), None);
        // A non-rate-limit error status is ignored.
        let other = json!({"apiErrorStatus": 400});
        assert_eq!(Provider::Glm.detect_disruption(&other), None);
        // A successful result with no error text.
        let ok = json!({"type": "result", "is_error": false, "result": "done"});
        assert_eq!(Provider::Glm.detect_disruption(&ok), None);
    }
}

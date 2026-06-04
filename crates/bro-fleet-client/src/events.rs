//! Stream-json event extraction.
//!
//! A provider-agnostic port of the daemon's `parse_claude_event`
//! (`orchestration/providers/events.rs`). Every surviving fleet provider
//! (GLM / DeepSeek / Brodex / VibeBh) emits the Claude stream-json envelope via
//! bro-harness, so the client needs exactly this one extractor — no per-provider
//! branching. The cockpit consumes last-assistant-message / cost / turn-count;
//! token `Usage` is not surfaced in the fleet view, so it is not tracked here.

use serde_json::Value;

/// Mutable state the event extractor fills from the stream-json envelope.
#[derive(Default)]
pub struct EventSink {
    pub last_assistant_message: Option<String>,
    pub cost_usd: Option<f64>,
    pub num_turns: Option<u64>,
    pub session_id: Option<String>,
}

fn append_block_separator(buf: &mut Option<String>) {
    if let Some(existing) = buf.as_mut()
        && !existing.is_empty()
    {
        existing.push_str("\n\n");
    }
}

/// Parse one Claude-envelope stream-json event and update the sink. Ported
/// verbatim from the daemon's `parse_claude_event` minus the token-`Usage`
/// accumulation the fleet view doesn't display.
pub fn parse_claude_event(evt: &Value, sink: &mut EventSink) {
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
        sink.cost_usd = evt["total_cost_usd"].as_f64();
        sink.num_turns = evt["num_turns"].as_u64();
    }
}

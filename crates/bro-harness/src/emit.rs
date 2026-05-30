//! Emits the exact Claude `stream-json` envelope the daemon's
//! `parse_claude_event` consumes (`src/orchestration/providers/events.rs`).
//! Every line carries a top-level `session_id` (the parser reads
//! `evt["session_id"]`). Only protocol JSON goes to stdout — all logging
//! goes to stderr.

use crate::transport::{ToolResult, Usage};
use serde_json::{Value, json};
use std::io::Write;

pub struct Emitter {
    session_id: String,
}

impl Emitter {
    pub fn new(session_id: String) -> Self {
        Self { session_id }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    fn write_line(&self, v: serde_json::Value) {
        let mut out = std::io::stdout().lock();
        // A failed write to stdout (closed pipe) is unrecoverable for the
        // protocol; ignore and let the process wind down.
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    }

    /// `{"type":"system","subtype":"init","session_id":...}` — establishes the
    /// session id for the daemon up front.
    pub fn system_init(&self) {
        self.write_line(json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.session_id,
        }));
    }

    /// `system/init` for bidirectional mode, advertising the in-stream slash
    /// commands the harness accepts (currently `/compact`) so a driver knows the
    /// control surface (NDJSON_FORMAT.md §system/init `slash_commands`).
    pub fn system_init_session(&self) {
        self.write_line(json!({
            "type": "system",
            "subtype": "init",
            "session_id": self.session_id,
            "slash_commands": ["compact"],
        }));
    }

    /// Re-emit a stdin user message as a `user` event (for
    /// `--replay-user-messages`), tagged `isReplay` so the driver can tell it
    /// from a fresh turn.
    pub fn replay_user(&self, message: &Value) {
        self.write_line(json!({
            "type": "user",
            "session_id": self.session_id,
            "isReplay": true,
            "message": message,
        }));
    }

    /// A successful `control_response` for a `control_request` (e.g. interrupt,
    /// set_model). The Claude Agent SDK shape: `response.{subtype,request_id}`.
    pub fn control_response_success(&self, request_id: Option<&str>) {
        self.write_line(json!({
            "type": "control_response",
            "response": {
                "subtype": "success",
                "request_id": request_id,
            },
        }));
    }

    /// One incremental streaming event — the inner Anthropic `event` wrapped as
    /// a Claude `stream_event` line. The daemon's parser folds
    /// `content_block_delta` text into the live assistant message and reads
    /// `message_delta` usage; the fleet TUI's live transcript reads the richer
    /// `content_block_start` (tool_use) and `input_json_delta` (args) too.
    pub fn stream_event(&self, event: Value) {
        self.write_line(json!({
            "type": "stream_event",
            "session_id": self.session_id,
            "event": event,
        }));
    }

    /// The full assistant turn as an Anthropic content array (text + thinking +
    /// tool_use blocks). When the turn streamed, the daemon dedupes the text
    /// against what it already accumulated from `stream_event` deltas
    /// (`streaming_captured` guard); the tool_use blocks are what surface tool
    /// calls to supervision and the fleet transcript. When the turn did NOT
    /// stream (non-streaming transports), this is the authoritative text.
    pub fn assistant_message(&self, content: Vec<Value>) {
        self.write_line(json!({
            "type": "assistant",
            "session_id": self.session_id,
            "message": {
                "role": "assistant",
                "content": content,
                "session_id": self.session_id,
            },
        }));
    }

    /// Tool results for the calls just dispatched, as a `user` turn of
    /// `tool_result` blocks (the Anthropic shape). The daemon's claude parser
    /// ignores `user` lines (no handler), so this is purely additive — it gives
    /// the fleet transcript the tool responses to render inline. Skips emission
    /// when there are no results.
    pub fn tool_results(&self, results: &[ToolResult]) {
        if results.is_empty() {
            return;
        }
        let blocks: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": r.content,
                    "is_error": r.is_error,
                })
            })
            .collect();
        self.write_line(json!({
            "type": "user",
            "session_id": self.session_id,
            "message": {
                "role": "user",
                "content": blocks,
                "session_id": self.session_id,
            },
        }));
    }

    /// Marks an auto-compaction boundary in the stream — the harness summarized
    /// and replaced the older conversation prefix. The daemon's claude parser
    /// ignores unknown `system` subtypes, so this is purely a marker for the
    /// fleet TUI transcript (rendered as a divider). `pre_tokens` is the prompt
    /// size that tripped the threshold.
    pub fn compact_boundary(&self, trigger: &str, pre_tokens: u64, summary_chars: usize) {
        self.write_line(json!({
            "type": "system",
            "subtype": "compact_boundary",
            "session_id": self.session_id,
            "compact_metadata": {
                "trigger": trigger,
                "pre_tokens": pre_tokens,
                "summary_chars": summary_chars,
            },
        }));
    }

    /// The builtin `report` tool's status/needs signal — drives the cockpit's
    /// Waiting bucket and row summary (fleet-tui.md §2.2). Distinct from the
    /// daemon's `bro_report`; the daemon's claude parser ignores `report` lines.
    pub fn report(&self, message: &str, needs_input: bool) {
        self.write_line(json!({
            "type": "report",
            "session_id": self.session_id,
            "report": {
                "message": message,
                "needs_input": needs_input,
            },
        }));
    }

    /// Terminal `result` event with usage/turns/cost.
    pub fn result(&self, text: &str, usage: &Usage, num_turns: u64, cost_usd: Option<f64>) {
        // Emit the Anthropic-native usage shape (fresh `input_tokens` plus
        // `cache_read_input_tokens` / `cache_creation_input_tokens`) so the
        // daemon's claude parser captures the cache breakdown identically to a
        // real Claude CLI run.
        let mut v = json!({
            "type": "result",
            "subtype": "success",
            "session_id": self.session_id,
            "result": text,
            "num_turns": num_turns,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
                "cache_read_input_tokens": usage.cached_input_tokens,
                "cache_creation_input_tokens": usage.cache_creation_input_tokens,
            },
        });
        if let Some(c) = cost_usd {
            v["total_cost_usd"] = json!(c);
        }
        self.write_line(v);
    }
}

impl crate::transport::TurnSink for Emitter {
    fn stream_event(&self, event: Value) {
        Emitter::stream_event(self, event);
    }
}

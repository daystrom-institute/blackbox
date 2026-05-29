//! Emits the exact Claude `stream-json` envelope the daemon's
//! `parse_claude_event` consumes (`src/orchestration/providers/events.rs`).
//! Every line carries a top-level `session_id` (the parser reads
//! `evt["session_id"]`). Only protocol JSON goes to stdout — all logging
//! goes to stderr.

use crate::transport::Usage;
use serde_json::json;
use std::io::Write;

pub struct Emitter {
    session_id: String,
}

impl Emitter {
    pub fn new(session_id: String) -> Self {
        Self { session_id }
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

    /// One assistant text turn. The parser uses this when no streamed
    /// `stream_event` text was captured — which is always true in the
    /// non-streaming cut.
    pub fn assistant_text(&self, text: &str) {
        self.write_line(json!({
            "type": "assistant",
            "session_id": self.session_id,
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": text}],
                "session_id": self.session_id,
            },
        }));
    }

    /// Terminal `result` event with usage/turns/cost.
    pub fn result(&self, text: &str, usage: &Usage, num_turns: u64, cost_usd: Option<f64>) {
        let mut v = json!({
            "type": "result",
            "subtype": "success",
            "session_id": self.session_id,
            "result": text,
            "num_turns": num_turns,
            "usage": {
                "input_tokens": usage.input_tokens,
                "output_tokens": usage.output_tokens,
            },
        });
        if let Some(c) = cost_usd {
            v["total_cost_usd"] = json!(c);
        }
        self.write_line(v);
    }
}

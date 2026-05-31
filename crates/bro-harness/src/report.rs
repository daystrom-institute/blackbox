//! Builtin `report` tool — the agent's status/needs signal for the fleet
//! cockpit (design/fleet-tui/fleet-tui.md §2.2).
//!
//! When the agent calls `report`, the harness emits a `report` stream-json line
//! that the cockpit reads to drive the **Waiting** bucket (when `needs_input`)
//! and the per-row summary. It is a harness builtin holding its own [`Emitter`]
//! handle, registered beside the bro-tools builtins; unpinned in normal CLI mode
//! and pinned in fleet mode (via `PinPolicy::also_pin`).
//!
//! It is **not** the daemon's `bro_report` MCP tool, which serves
//! `bro_exec`/atom/workflow bros and is surface-gated off fleet agents. No
//! shared field — fleet agents are not bros.

use crate::emit::Emitter;
use async_trait::async_trait;
use bro_tools::{Tool, ToolAnnotations, ToolCx, ToolResult};
use serde_json::{Value, json};

pub const REPORT_TOOL: &str = "report";

pub struct ReportTool {
    emitter: Emitter,
}

impl ReportTool {
    pub fn new(emitter: Emitter) -> Self {
        Self { emitter }
    }
}

#[async_trait]
impl Tool for ReportTool {
    fn name(&self) -> &str {
        REPORT_TOOL
    }
    fn description(&self) -> &str {
        "Report your current status to the operator's cockpit and flag whether you need input. \
         `message` is a one-line status, progress note, blocker, or question; set `needs_input` \
         true when you are waiting on the operator before you can continue. This is a status \
         signal surfaced live — it does not replace doing the work, and it returns nothing useful \
         to act on."
    }
    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "string",
                    "description": "One-line status, progress note, blocker, or question."
                },
                "needs_input": {
                    "type": "boolean",
                    "description": "True if you are blocked waiting on the operator."
                }
            },
            "required": ["message"]
        })
    }
    fn annotations(&self) -> ToolAnnotations {
        ToolAnnotations {
            read_only: true,
            destructive: false,
        }
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let message = input["message"].as_str().unwrap_or("").trim().to_string();
        if message.is_empty() {
            return ToolResult::Error("report requires a non-empty `message`".into());
        }
        let needs_input = input["needs_input"].as_bool().unwrap_or(false);
        self.emitter.report(&message, needs_input);
        ToolResult::Json(json!({"ok": true, "reported": message, "needs_input": needs_input}))
    }
}

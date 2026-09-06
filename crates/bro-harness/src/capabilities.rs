//! Harness-local capability adapters.
//!
//! The daemon runs the harness as an independent process, so daemon-owned
//! corpus and atom capabilities arrive through its MCP catalog. This module
//! retains only the generic [`HostTools`] seam used to project the already
//! filtered session tool set into code-mode cells.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{ToolCallOutput, ToolCapability, ToolInvocation};
use bro_core::BroError;
use bro_tools::{Tool, ToolCx, ToolResult};

/// The generic host built-in tool seam: a code-mode cell's `tools.*` call
/// dispatches here, and this runs the matching bro-tools built-in by name
/// against the per-session [`ToolCx`] — the same `Tool::call` path the flat
/// model-facing surface uses.
///
/// Deny-filter invariant: the callable set is gated by the **same** `ToolFilter`
/// as the flat surface (an unfiltered in-box surface would be a deny-bypass). The
/// caller constructs `HostTools` from the already-filtered built-in set, so a
/// denied capability is absent here and fails closed.
pub struct HostTools {
    tools: HashMap<String, Arc<dyn Tool>>,
    cx: ToolCx,
}

impl HostTools {
    /// Build the host-tool seam from a pre-filtered built-in set + the session
    /// context. `filtered_builtins` MUST already have had the session's
    /// `ToolFilter` applied by the caller; capability/control tools
    /// (`exec`, `wait`, `report`, …) are intentionally NOT
    /// included — they are model-facing controls, not nested cell tools.
    pub fn new(filtered_builtins: Vec<Arc<dyn Tool>>, cx: ToolCx) -> Self {
        let tools = filtered_builtins
            .into_iter()
            .map(|t| (t.name().to_string(), t))
            .collect();
        Self { tools, cx }
    }
}

#[async_trait]
impl ToolCapability for HostTools {
    async fn call_tool(&self, invocation: ToolInvocation) -> Result<ToolCallOutput, BroError> {
        let tool = self.tools.get(&invocation.name).ok_or_else(|| {
            // Unknown OR filtered-out → fail closed (no in-box route around the
            // ToolFilter, §4.5).
            BroError::new(
                "tool_unavailable",
                format!(
                    "host tool '{}' is not available in-box (unknown or denied)",
                    invocation.name
                ),
            )
        })?;
        let (content, is_error, content_type) = match crate::registry::call_tool_with_arg_defaults(
            tool.as_ref(),
            &invocation.name,
            invocation.input_json,
            &self.cx,
        )
        .await
        {
            ToolResult::Text(t) => (t, false, "text/plain"),
            ToolResult::Json(v) => (
                serde_json::to_string(&v).unwrap_or_else(|_| v.to_string()),
                false,
                "application/json",
            ),
            ToolResult::Error(e) => (e, true, "text/plain"),
        };
        Ok(ToolCallOutput {
            content,
            is_error,
            content_type: content_type.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn test_cx() -> ToolCx {
        use std::sync::Mutex;
        // A minimal context is sufficient for host-tool projection tests.
        ToolCx {
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    #[tokio::test]
    async fn host_tools_filtered_set_fails_closed_on_denied() {
        // HostTools built from a filtered built-in set: file_read survives, but a
        // tool excluded by the filter (e.g. shell_run denied) is absent → calling
        // it in-box fails closed (no deny-bypass, §4.5).
        let filter = crate::mcp::ToolFilter::from_csv(Some("shell_run"), None);
        let allowed: Vec<Arc<dyn Tool>> = bro_tools::builtin_tools()
            .into_iter()
            .filter(|t| filter.permits(t.name()))
            .collect();
        let host = HostTools::new(allowed, test_cx());

        // file_read is permitted (no real file needed — it returns a tool error
        // for a missing path, which is is_error=true, NOT tool_unavailable).
        let read = host
            .call_tool(ToolInvocation {
                name: "file_read".to_string(),
                input_json: json!({ "file_path": "definitely-missing.xyz" }),
            })
            .await
            .expect("file_read is in the filtered set");
        assert!(read.is_error, "missing file → tool-level error");

        // shell_run was denied → absent from the in-box set → fail closed.
        let denied = host
            .call_tool(ToolInvocation {
                name: "shell_run".to_string(),
                input_json: json!({ "command": "echo nope" }),
            })
            .await;
        let err = denied.expect_err("denied tool must fail closed");
        assert_eq!(err.code, "tool_unavailable");
    }
}

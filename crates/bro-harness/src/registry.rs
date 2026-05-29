//! Tool registry: client tools dispatched in-process, exposed to the loop as
//! transport-agnostic [`ToolSpec`]s. Server-side tools (web search) are NOT
//! here — each transport injects its own when `TurnOpts.web_search` is set.

use crate::transport::ToolSpec;
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

pub struct Registry {
    client: HashMap<String, Arc<dyn Tool>>,
}

impl Registry {
    pub fn new(client_tools: Vec<Arc<dyn Tool>>) -> Self {
        let mut client = HashMap::new();
        for t in client_tools {
            // Last writer wins; MCP tools added after built-ins may shadow.
            client.insert(t.name().to_string(), t);
        }
        Self { client }
    }

    /// Normalized tool definitions for the transport to render to wire shape.
    pub fn tool_specs(&self) -> Vec<ToolSpec> {
        self.client
            .values()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.input_schema(),
            })
            .collect()
    }

    pub async fn dispatch(&self, name: &str, input: Value, cx: &ToolCx) -> ToolResult {
        match self.client.get(name) {
            Some(tool) => tool.call(input, cx).await,
            None => ToolResult::Error(format!("unknown tool: {name}")),
        }
    }
}

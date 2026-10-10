//! Typed settings for the execution-host Codex app-server adapter.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodexSessionConfig {
    pub resume: Option<String>,
    pub model: Option<String>,
    pub effort: Option<String>,
    pub service_tier: Option<String>,
    pub developer_instructions: Option<String>,
    pub output_schema: Option<Value>,
    /// MCP servers resolved by dispatch, before native config conversion.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, Value>,
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    #[serde(default)]
    pub disallowed_tools: Vec<String>,
    /// Composition errors are refused before a process is spawned.
    #[serde(default)]
    pub errors: Vec<String>,
}

impl std::fmt::Debug for CodexSessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexSessionConfig")
            .field("resume", &self.resume)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("service_tier", &self.service_tier)
            .field("mcp_servers", &self.mcp_servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

/// One command delivered to the native Codex adapter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexInput {
    pub command: crate::SessionCommand,
    #[serde(default)]
    pub request_id: Option<String>,
}

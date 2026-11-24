//! Typed settings for the execution-host Antigravity ACP adapter.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AgySessionConfig {
    /// The session to continue; `None` starts a new one.
    pub resume: Option<String>,
    /// Value for the session's `model` option. The server does not carry it
    /// into a new process, so a resume names it too.
    pub model: Option<String>,
    /// Value for the session's `thought_level` option, applied after the
    /// model because each model offers its own levels.
    pub effort: Option<String>,
    /// Text appended to the agent's system prompt. The server keeps it with
    /// the session; sending it again on resume does not repeat it.
    pub system_prompt_append: Option<String>,
    /// MCP servers resolved by dispatch, attached to this session only. A
    /// header or env value of the form `$env:NAME` names a variable of the
    /// worker environment.
    #[serde(default)]
    pub mcp_servers: BTreeMap<String, Value>,
    /// Composition errors are refused before a process is spawned.
    #[serde(default)]
    pub errors: Vec<String>,
}

impl std::fmt::Debug for AgySessionConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgySessionConfig")
            .field("resume", &self.resume)
            .field("model", &self.model)
            .field("effort", &self.effort)
            .field("mcp_servers", &self.mcp_servers.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

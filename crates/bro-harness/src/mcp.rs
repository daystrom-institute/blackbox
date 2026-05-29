//! MCP client: connect to the transient blackbox server the daemon injects
//! via `--mcp-config` and expose its tools as `Tool` impls.
//!
//! SKELETON: wiring is stubbed. The real implementation parses the
//! `{"mcpServers":{name:{url}}}` config, dials the Streamable-HTTP endpoint
//! with `rmcp`'s client transport (already a dependency of the daemon),
//! lists tools, and wraps each `rmcp` tool as a `bro_tools::Tool` whose
//! `call` proxies a `tools/call` JSON-RPC request.

use bro_tools::Tool;
use std::sync::Arc;

/// Parse `--mcp-config` and return wrapped MCP tools. Currently returns an
/// empty set and logs, so the loop runs with built-in + server-side tools
/// only until the rmcp client wiring lands.
pub async fn load_mcp_tools(mcp_config: Option<&str>) -> Vec<Arc<dyn Tool>> {
    match mcp_config {
        Some(cfg) => {
            tracing::warn!(
                config_len = cfg.len(),
                "MCP tool loading not yet wired; ignoring --mcp-config for now"
            );
            Vec::new()
        }
        None => Vec::new(),
    }
}

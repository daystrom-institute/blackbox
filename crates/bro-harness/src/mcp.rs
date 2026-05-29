//! MCP client: connect to the MCP server(s) the daemon injects via
//! `--mcp-config` and expose their tools as `bro_tools::Tool` impls, merged
//! into the registry alongside the built-in workspace/web tools.
//!
//! Transport + call pattern mirror the daemon's own outbound client
//! (`src/mcp_client.rs`): rmcp 1.4 `StreamableHttpClientTransport::from_uri`
//! with `().serve(transport)`. Connections are per-operation (one to list at
//! startup, one per tool call) — simple and correct for a short-lived
//! harness against a loopback server; a pooled/persistent connection is a
//! later optimization.
//!
//! Failures are best-effort: a server that can't be reached or listed is
//! logged (to stderr) and skipped — MCP unavailability never aborts the
//! harness.

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, RawContent};
use rmcp::transport::StreamableHttpClientTransport;
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

/// Parse `--mcp-config` (`{"mcpServers":{name:{"type":"http","url":...}}}`),
/// connect to each server, and return its tools wrapped as `Tool` impls.
pub async fn load_mcp_tools(mcp_config: Option<&str>) -> Vec<Arc<dyn Tool>> {
    let Some(cfg) = mcp_config else {
        return Vec::new();
    };
    let servers = match parse_servers(cfg) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("ignoring --mcp-config (parse failed): {e:#}");
            return Vec::new();
        }
    };

    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for (name, url) in servers {
        match list_server_tools(&url).await {
            Ok(listed) => {
                tracing::info!(server = %name, count = listed.len(), "MCP tools loaded");
                for t in listed {
                    tools.push(Arc::new(t));
                }
            }
            Err(e) => tracing::warn!(server = %name, "MCP tool listing failed: {e:#}"),
        }
    }
    tools
}

fn parse_servers(cfg: &str) -> anyhow::Result<Vec<(String, String)>> {
    let v: Value = serde_json::from_str(cfg)?;
    let mut out = Vec::new();
    if let Some(obj) = v["mcpServers"].as_object() {
        for (name, sc) in obj {
            if let Some(url) = sc["url"].as_str() {
                out.push((name.clone(), url.to_string()));
            }
        }
    }
    Ok(out)
}

async fn list_server_tools(url: &str) -> anyhow::Result<Vec<McpTool>> {
    let transport = StreamableHttpClientTransport::from_uri(url.to_string());
    let client = ().serve(transport).await?;
    let listed = client.list_all_tools().await;
    let mut client = client;
    let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    let listed = listed?;
    Ok(listed
        .into_iter()
        .map(|t| McpTool {
            url: url.to_string(),
            name: t.name.to_string(),
            description: t.description.map(|d| d.to_string()).unwrap_or_default(),
            schema: Value::Object((*t.input_schema).clone()),
        })
        .collect())
}

/// A single MCP tool, dispatched by re-dialing its server per call.
struct McpTool {
    url: String,
    name: String,
    description: String,
    schema: Value,
}

#[async_trait]
impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.name
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn input_schema(&self) -> Value {
        self.schema.clone()
    }
    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        match self.call_inner(input).await {
            Ok(r) => r,
            Err(e) => ToolResult::Error(format!("mcp call '{}' failed: {e:#}", self.name)),
        }
    }
}

impl McpTool {
    async fn call_inner(&self, input: Value) -> anyhow::Result<ToolResult> {
        let transport = StreamableHttpClientTransport::from_uri(self.url.clone());
        let client = ().serve(transport).await?;
        // Always send an arguments object (even empty) — some servers reject a
        // missing `arguments` field with -32602.
        let args = match input {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let params =
            CallToolRequestParams::new(self.name.clone()).with_arguments(args.into_iter().collect());
        let resp = client.call_tool(params).await;
        let mut client = client;
        let _ = client.close_with_timeout(Duration::from_secs(2)).await;
        Ok(to_tool_result(resp?))
    }
}

fn to_tool_result(r: CallToolResult) -> ToolResult {
    let text = collect_text(&r.content);
    if r.is_error.unwrap_or(false) {
        return ToolResult::Error(if text.is_empty() {
            "tool returned is_error=true with no text".to_string()
        } else {
            text
        });
    }
    if let Some(sc) = r.structured_content {
        return ToolResult::Json(sc);
    }
    ToolResult::Text(text)
}

fn collect_text(content: &[Content]) -> String {
    let mut out = String::new();
    for c in content {
        if let RawContent::Text(t) = &c.raw {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&t.text);
        }
    }
    out
}

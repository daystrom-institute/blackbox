//! MCP client: connect to the MCP server(s) injected via `--mcp-config` and
//! expose their tools as `bro_tools::Tool` impls, merged into the registry
//! alongside the built-in workspace/web tools.
//!
//! Transport + call pattern mirror the daemon's own outbound client
//! (`src/mcp_client.rs`): streamable HTTP uses
//! `StreamableHttpClientTransport::from_uri`, and stdio uses rmcp's
//! `TokioChildProcess`. Connections are per-operation (one to list at startup,
//! one per tool call) — simple and correct for short-lived harness sessions; a
//! pooled/persistent connection is a later optimization.
//!
//! Failures are best-effort: a server that can't be reached or listed is
//! logged (to stderr) and skipped — MCP unavailability never aborts the
//! harness.

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, RawContent};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// Parse `--mcp-config` (`{"mcpServers":{name:{...}}}`), connect to each
/// server, list its tools, and return those admitted by `filter`. Denied tools
/// are dropped here so they never enter the registry — not listed to the model,
/// not loadable via tool_search, not dispatchable.
pub async fn load_mcp_tools(mcp_config: Option<&str>, filter: &ToolFilter) -> Vec<Arc<dyn Tool>> {
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
    for (name, transport) in servers {
        match list_server_tools(&name, &transport).await {
            Ok(listed) => {
                let total = listed.len();
                let mut admitted = 0;
                for t in listed {
                    if filter.permits(t.name()) {
                        admitted += 1;
                        tools.push(Arc::new(t));
                    }
                }
                tracing::info!(
                    server = %name,
                    admitted,
                    denied = total - admitted,
                    "MCP tools loaded"
                );
            }
            Err(e) => tracing::warn!(server = %name, "MCP tool listing failed: {e:#}"),
        }
    }
    tools
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPlacement {
    InBox,
    OutBox,
    Both,
}

impl ToolPlacement {
    pub fn in_box(self) -> bool {
        matches!(self, Self::InBox | Self::Both)
    }

    pub fn out_box(self) -> bool {
        matches!(self, Self::OutBox | Self::Both)
    }
}

pub type ToolPlacementMap = BTreeMap<String, ToolPlacement>;
pub type ToolList = Vec<Arc<dyn Tool>>;

/// Parse the top-level `tool_placement` map from the same JSON blob that carries
/// `mcpServers`. Missing and invalid entries fail safe to the default out-box
/// placement; the caller applies this map only after [`ToolFilter`] admission.
pub fn parse_tool_placement(mcp_config: Option<&str>) -> ToolPlacementMap {
    let Some(cfg) = mcp_config else {
        return ToolPlacementMap::new();
    };
    let v: Value = match serde_json::from_str(cfg) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("ignoring tool_placement (MCP config parse failed): {e:#}");
            return ToolPlacementMap::new();
        }
    };
    let mut out = ToolPlacementMap::new();
    let Some(obj) = v.get("tool_placement").and_then(Value::as_object) else {
        return out;
    };
    for (name, placement) in obj {
        let Some(placement) = placement.as_str() else {
            tracing::warn!(tool = %name, "ignoring non-string tool_placement entry");
            continue;
        };
        let parsed = match placement {
            "in-box" => ToolPlacement::InBox,
            "out-box" => ToolPlacement::OutBox,
            "both" => ToolPlacement::Both,
            other => {
                tracing::warn!(tool = %name, placement = %other, "ignoring unknown tool_placement");
                continue;
            }
        };
        out.insert(name.clone(), parsed);
    }
    out
}

pub fn split_mcp_tools_by_placement(
    mcp_tools: &[Arc<dyn Tool>],
    placements: &ToolPlacementMap,
) -> (ToolList, ToolList) {
    let mut in_box = Vec::new();
    let mut out_box = Vec::new();
    for tool in mcp_tools {
        let placement = placements
            .get(tool.name())
            .copied()
            .unwrap_or(ToolPlacement::OutBox);
        if placement.in_box() {
            in_box.push(tool.clone());
        }
        if placement.out_box() {
            out_box.push(tool.clone());
        }
    }
    (in_box, out_box)
}

/// Client-side allow/deny over the whole tool surface — the permission plane
/// (recursion guard + brofile + per-dispatch), distinct from server-side
/// surface. Built from the daemon's `--deny-tools`/`--allow-tools` flags.
/// Patterns are exact names or a trailing-`*` prefix glob, matched against the
/// MCP tools' fully-qualified `mcp__<server>__<tool>` names AND built-in tools'
/// bare names (`shell_run`, `git_*`, …). This is the final lever when nudges
/// aren't enough: force MCP pathways, deny a dumb drone `shell_*`, stop an
/// Explore agent from `file_edit`, etc.
#[derive(Default)]
pub struct ToolFilter {
    deny: Vec<String>,
    allow: Vec<String>,
}

impl ToolFilter {
    pub fn from_csv(deny: Option<&str>, allow: Option<&str>) -> Self {
        fn split(s: Option<&str>) -> Vec<String> {
            s.into_iter()
                .flat_map(|v| v.split(','))
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect()
        }
        Self {
            deny: split(deny),
            allow: split(allow),
        }
    }

    /// True if `name` matches an explicit deny pattern. Deny-only check, used
    /// for tools that should ignore the allow-list exclusion but still honor a
    /// targeted deny (e.g. `tool_search`).
    pub fn denied(&self, name: &str) -> bool {
        self.deny.iter().any(|p| pattern_matches(p, name))
    }

    /// A tool name is permitted unless it matches a deny pattern, or (when allow
    /// is non-empty) fails to match any allow pattern. Deny wins.
    pub fn permits(&self, name: &str) -> bool {
        if self.denied(name) {
            return false;
        }
        if !self.allow.is_empty() && !self.allow.iter().any(|p| pattern_matches(p, name)) {
            return false;
        }
        true
    }
}

fn pattern_matches(pattern: &str, name: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == pattern,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum McpTransportConfig {
    Http {
        url: String,
    },
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

fn parse_servers(cfg: &str) -> anyhow::Result<Vec<(String, McpTransportConfig)>> {
    let v: Value = serde_json::from_str(cfg)?;
    let mut out = Vec::new();
    if let Some(obj) = v["mcpServers"].as_object() {
        for (name, sc) in obj {
            let transport_type = sc
                .get("type")
                .and_then(|t| t.as_str())
                .or_else(|| sc.get("command").map(|_| "stdio"))
                .or_else(|| sc.get("url").map(|_| "http"));
            let Some(transport_type) = transport_type else {
                tracing::warn!(server = %name, "ignoring MCP server with no transport fields");
                continue;
            };
            match transport_type {
                "http" | "sse" => {
                    if let Some(url) = sc["url"].as_str() {
                        out.push((
                            name.clone(),
                            McpTransportConfig::Http {
                                url: url.to_string(),
                            },
                        ));
                    } else {
                        tracing::warn!(server = %name, "ignoring MCP server with no url");
                    }
                }
                "stdio" => {
                    if let Some(command) = sc["command"].as_str() {
                        out.push((
                            name.clone(),
                            McpTransportConfig::Stdio {
                                command: command.to_string(),
                                args: parse_string_array(sc.get("args")),
                                env: parse_string_map(sc.get("env")),
                            },
                        ));
                    } else {
                        tracing::warn!(server = %name, "ignoring stdio MCP server with no command");
                    }
                }
                other => {
                    tracing::warn!(server = %name, transport = %other, "ignoring unsupported MCP transport");
                }
            }
        }
    }
    Ok(out)
}

fn parse_string_array(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect()
}

fn parse_string_map(v: Option<&Value>) -> BTreeMap<String, String> {
    v.and_then(|v| v.as_object())
        .into_iter()
        .flatten()
        .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
        .collect()
}

async fn list_server_tools(
    server_name: &str,
    transport: &McpTransportConfig,
) -> anyhow::Result<Vec<McpTool>> {
    let listed = match transport {
        McpTransportConfig::Http { url } => list_http_server_tools(url).await?,
        McpTransportConfig::Stdio { command, args, env } => {
            list_stdio_server_tools(command, args, env).await?
        }
    };
    Ok(listed
        .into_iter()
        .map(|t| McpTool {
            transport: transport.clone(),
            call_name: t.name.to_string(),
            name: format!("mcp__{server_name}__{}", t.name),
            description: t.description.map(|d| d.to_string()).unwrap_or_default(),
            schema: Value::Object((*t.input_schema).clone()),
        })
        .collect())
}

async fn list_http_server_tools(url: &str) -> anyhow::Result<Vec<rmcp::model::Tool>> {
    let transport = StreamableHttpClientTransport::from_uri(url.to_string());
    let client = ().serve(transport).await?;
    let listed = client.list_all_tools().await;
    let mut client = client;
    let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    Ok(listed?)
}

async fn list_stdio_server_tools(
    command: &str,
    args: &[String],
    env: &BTreeMap<String, String>,
) -> anyhow::Result<Vec<rmcp::model::Tool>> {
    let mut command_builder = tokio::process::Command::new(command);
    command_builder.args(args);
    command_builder.envs(env);
    let transport = TokioChildProcess::new(command_builder.configure(|_| {}))?;
    let client = ().serve(transport).await?;
    let listed = client.list_all_tools().await;
    let mut client = client;
    let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    Ok(listed?)
}

/// A single MCP tool, dispatched by re-dialing its server per call.
struct McpTool {
    transport: McpTransportConfig,
    call_name: String,
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
        // Always send an arguments object (even empty) — some servers reject a
        // missing `arguments` field with -32602.
        let args = match input {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let params = CallToolRequestParams::new(self.call_name.clone())
            .with_arguments(args.into_iter().collect());
        let resp = match &self.transport {
            McpTransportConfig::Http { url } => {
                let transport = StreamableHttpClientTransport::from_uri(url.clone());
                let client = ().serve(transport).await?;
                let resp = client.call_tool(params).await;
                let mut client = client;
                let _ = client.close_with_timeout(Duration::from_secs(2)).await;
                resp
            }
            McpTransportConfig::Stdio { command, args, env } => {
                let mut command_builder = tokio::process::Command::new(command);
                command_builder.args(args);
                command_builder.envs(env);
                let transport = TokioChildProcess::new(command_builder.configure(|_| {}))?;
                let client = ().serve(transport).await?;
                let resp = client.call_tool(params).await;
                let mut client = client;
                let _ = client.close_with_timeout(Duration::from_secs(2)).await;
                resp
            }
        };
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

#[cfg(test)]
mod tests {
    use super::*;

    fn mock_tool(name: &'static str) -> Arc<dyn Tool> {
        struct T(&'static str);
        #[async_trait]
        impl Tool for T {
            fn name(&self) -> &str {
                self.0
            }
            fn description(&self) -> &str {
                "mock"
            }
            fn input_schema(&self) -> Value {
                serde_json::json!({"type": "object", "properties": {}})
            }
            async fn call(&self, _input: Value, _cx: &ToolCx) -> ToolResult {
                ToolResult::Text("ok".into())
            }
        }
        Arc::new(T(name))
    }

    #[test]
    fn tool_filter_deny_blocks_recursion_guard_patterns() {
        // What the daemon's recursion guard emits for a harness provider.
        let f = ToolFilter::from_csv(
            Some("mcp__blackbox__bro_exec,mcp__blackbox__bro_resume,mcp__blackbox__bro_*"),
            None,
        );
        assert!(!f.permits("mcp__blackbox__bro_exec"));
        assert!(!f.permits("mcp__blackbox__bro_resume"));
        assert!(!f.permits("mcp__blackbox__bro_cancel")); // matched by bro_*
        // Pinned/allowed tools survive.
        assert!(f.permits("mcp__blackbox__bbox_slice_read"));
        assert!(f.permits("mcp__blackbox__bbox_stats"));
        // Built-in (non-MCP-qualified) names are never matched by these.
        assert!(f.permits("file_read"));
    }

    #[test]
    fn tool_filter_allowlist_is_exclusive() {
        let f = ToolFilter::from_csv(None, Some("mcp__blackbox__bbox_*"));
        assert!(f.permits("mcp__blackbox__bbox_stats"));
        assert!(!f.permits("mcp__blackbox__bro_status")); // not in allow
    }

    #[test]
    fn tool_filter_empty_permits_all() {
        let f = ToolFilter::from_csv(None, None);
        assert!(f.permits("mcp__blackbox__bro_exec"));
        assert!(f.permits("anything"));
    }

    #[test]
    fn tool_placement_parses_and_defaults_out_box() {
        let placements = parse_tool_placement(Some(
            r#"{
                "mcpServers": {},
                "tool_placement": {
                    "mcp__blackbox__bbox_code_node_describe": "in-box",
                    "mcp__blackbox__bbox_hybrid_search": "out-box",
                    "mcp__blackbox__bbox_slice_read": "both"
                }
            }"#,
        ));
        assert_eq!(
            placements.get("mcp__blackbox__bbox_code_node_describe"),
            Some(&ToolPlacement::InBox)
        );
        assert_eq!(
            placements.get("mcp__blackbox__bbox_hybrid_search"),
            Some(&ToolPlacement::OutBox)
        );
        assert_eq!(
            placements.get("mcp__blackbox__bbox_slice_read"),
            Some(&ToolPlacement::Both)
        );
        assert_eq!(placements.get("mcp__blackbox__unlisted"), None);

        let tools = vec![
            mock_tool("mcp__blackbox__bbox_code_node_describe"),
            mock_tool("mcp__blackbox__bbox_hybrid_search"),
            mock_tool("mcp__blackbox__bbox_slice_read"),
            mock_tool("mcp__blackbox__unlisted"),
        ];
        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &placements);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(
            in_names,
            vec![
                "mcp__blackbox__bbox_code_node_describe",
                "mcp__blackbox__bbox_slice_read"
            ]
        );
        assert_eq!(
            out_names,
            vec![
                "mcp__blackbox__bbox_hybrid_search",
                "mcp__blackbox__bbox_slice_read",
                "mcp__blackbox__unlisted"
            ]
        );
    }

    #[test]
    fn parse_servers_accepts_stdio_entries() {
        let parsed = parse_servers(
            r#"{
                "mcpServers": {
                    "local_tools": {
                        "type": "stdio",
                        "command": "/opt/bin/local-tools",
                        "args": ["--scope", "probe"],
                        "env": {"LOCAL_TOOLS_SCOPE": "probe"}
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            parsed,
            vec![(
                "local_tools".to_string(),
                McpTransportConfig::Stdio {
                    command: "/opt/bin/local-tools".to_string(),
                    args: vec!["--scope".to_string(), "probe".to_string()],
                    env: BTreeMap::from([("LOCAL_TOOLS_SCOPE".to_string(), "probe".to_string())]),
                },
            )]
        );
    }

    #[test]
    fn parse_servers_preserves_url_entries() {
        let parsed = parse_servers(
            r#"{
                "mcpServers": {
                    "blackbox": {
                        "type": "http",
                        "url": "http://127.0.0.1:7264/mcp"
                    }
                }
            }"#,
        )
        .unwrap();

        assert_eq!(
            parsed,
            vec![(
                "blackbox".to_string(),
                McpTransportConfig::Http {
                    url: "http://127.0.0.1:7264/mcp".to_string(),
                },
            )]
        );
    }
}

//! MCP client: connect to typed or CLI-injected MCP server config and expose
//! their tools as `bro_tools::Tool` impls, merged into the registry alongside
//! the built-in workspace/web tools.
//!
//! Transport + call pattern mirror the daemon's own outbound client: streamable
//! HTTP uses rmcp's reqwest transport, and stdio uses rmcp's `TokioChildProcess`.
//! Connections are **persistent per server**: one connection is started when a
//! server's tools are loaded and Arc-shared by every `McpTool` it produces, so a
//! stateful server (e.g. `@playwright/mcp` holding a browser across calls) sees
//! the same session on every call rather than a fresh subprocess per call.
//! Dropping the last tool drops the connection; stdio children are reaped via
//! `kill_on_drop(true)`.
//!
//! Failures are best-effort: a server that can't be reached or listed is
//! logged (to stderr) and skipped — MCP unavailability never aborts the
//! harness.

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use http::{HeaderName, HeaderValue};
use rmcp::RoleClient;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, RawContent};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

#[derive(Clone)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    pub tool_placement: ToolPlacementMap,
}

impl McpConfig {
    pub fn from_json(cfg: &str) -> anyhow::Result<Self> {
        let v: Value = serde_json::from_str(cfg)?;
        let mut servers = Vec::new();
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
                    "http" => {
                        if let Some(url) = sc["url"].as_str() {
                            servers.push(McpServerConfig::Http {
                                name: name.clone(),
                                url: url.to_string(),
                                headers: parse_string_map(sc.get("headers")),
                                exclude_tools: parse_string_array(sc.get("exclude_tools")),
                            });
                        } else {
                            tracing::warn!(server = %name, "ignoring MCP server with no url");
                        }
                    }
                    "sse" => {
                        if let Some(url) = sc["url"].as_str() {
                            servers.push(McpServerConfig::Sse {
                                name: name.clone(),
                                url: url.to_string(),
                                headers: parse_string_map(sc.get("headers")),
                                exclude_tools: parse_string_array(sc.get("exclude_tools")),
                            });
                        } else {
                            tracing::warn!(server = %name, "ignoring MCP server with no url");
                        }
                    }
                    "stdio" => {
                        if let Some(command) = sc["command"].as_str() {
                            servers.push(McpServerConfig::Stdio {
                                name: name.clone(),
                                command: command.to_string(),
                                args: parse_string_array(sc.get("args")),
                                env: parse_string_map(sc.get("env")),
                            });
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
        Ok(Self {
            servers,
            tool_placement: parse_tool_placement_value(&v),
        })
    }
}

#[derive(Clone)]
pub enum McpServerConfig {
    Http {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
        exclude_tools: Vec<String>,
    },
    Sse {
        name: String,
        url: String,
        headers: BTreeMap<String, String>,
        exclude_tools: Vec<String>,
    },
    Stdio {
        name: String,
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
    },
    InProcess {
        name: String,
        server: Arc<dyn McpSurface>,
    },
}

impl McpServerConfig {
    pub fn name(&self) -> &str {
        match self {
            Self::Http { name, .. }
            | Self::Sse { name, .. }
            | Self::Stdio { name, .. }
            | Self::InProcess { name, .. } => name,
        }
    }

    fn excludes(&self, local_name: &str, qualified_name: &str) -> bool {
        let exclude_tools = match self {
            Self::Http { exclude_tools, .. } | Self::Sse { exclude_tools, .. } => exclude_tools,
            Self::Stdio { .. } | Self::InProcess { .. } => return false,
        };
        exclude_tools
            .iter()
            .any(|p| pattern_matches(p, local_name) || pattern_matches(p, qualified_name))
    }
}

#[derive(Debug, Clone)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[async_trait]
pub trait McpSurface: Send + Sync {
    async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>>;
    async fn call_tool(&self, tool: &str, input: Value) -> anyhow::Result<ToolResult>;
}

/// Parse `--mcp-config` (`{"mcpServers":{name:{...}}}`), connect to each
/// server, list its tools, and return those admitted by `filter`. Denied tools
/// are dropped here so they never enter the registry — not listed to the model,
/// not loadable via tool_search, not dispatchable.
pub async fn load_mcp_tools(mcp_config: Option<&str>, filter: &ToolFilter) -> Vec<Arc<dyn Tool>> {
    let Some(cfg) = mcp_config else {
        return Vec::new();
    };
    let config = match McpConfig::from_json(cfg) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("ignoring --mcp-config (parse failed): {e:#}");
            return Vec::new();
        }
    };
    load_mcp_tools_from_config(&config, filter).await
}

pub async fn load_mcp_tools_from_config(
    config: &McpConfig,
    filter: &ToolFilter,
) -> Vec<Arc<dyn Tool>> {
    let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
    for server in &config.servers {
        match server_backend_and_specs(server).await {
            Ok((backend, specs)) => {
                let total = specs.len();
                let mut admitted = 0;
                for (call_name, description, schema) in specs {
                    let qname = format!("mcp__{}__{}", server.name(), call_name);
                    if !server.excludes(&call_name, &qname) && filter.permits(&qname) {
                        admitted += 1;
                        tools.push(Arc::new(McpTool {
                            backend: backend.clone(),
                            call_name,
                            name: qname,
                            description,
                            schema,
                        }));
                    }
                }
                tracing::info!(
                    server = %server.name(),
                    admitted,
                    denied = total - admitted,
                    "MCP tools loaded"
                );
            }
            Err(e) => tracing::warn!(server = %server.name(), "MCP server unavailable: {e:#}"),
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
    parse_tool_placement_value(&v)
}

fn parse_tool_placement_value(v: &Value) -> ToolPlacementMap {
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

/// A persistent connection to one MCP server. Started once when its tools are
/// loaded and Arc-shared by every `McpTool` it produces, so a stateful server
/// (e.g. `@playwright/mcp` holding a browser across calls) sees the same
/// session on every call instead of a fresh subprocess. Dropping the last tool
/// drops the connection; stdio children are reaped via `kill_on_drop(true)`.
struct ServerConn {
    running: RunningService<RoleClient, ()>,
}

impl ServerConn {
    async fn list_tools(&self) -> anyhow::Result<Vec<rmcp::model::Tool>> {
        Ok(self.running.peer().list_all_tools().await?)
    }
    async fn call_tool(&self, params: CallToolRequestParams) -> anyhow::Result<CallToolResult> {
        Ok(self.running.peer().call_tool(params).await?)
    }
}

/// The shared backend an `McpTool` dispatches through. `Remote` is one
/// persistent rmcp connection; `InProcess` is a shared `McpSurface` (already
/// session-stable by construction). Cloned cheaply into each tool of a server.
#[derive(Clone)]
enum McpBackend {
    Remote(Arc<ServerConn>),
    InProcess(Arc<dyn McpSurface>),
}

/// Start one persistent connection to a remote (stdio/http/sse) MCP server.
/// InProcess servers have no rmcp connection and are handled by the caller.
async fn start_remote_server(server: &McpServerConfig) -> anyhow::Result<Arc<ServerConn>> {
    let running = match server {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).envs(env).kill_on_drop(true);
            let transport = TokioChildProcess::new(cmd.configure(|_| {}))?;
            ().serve(transport).await?
        }
        McpServerConfig::Http { url, headers, .. } | McpServerConfig::Sse { url, headers, .. } => {
            let transport =
                StreamableHttpClientTransport::from_config(http_transport_config(url, headers)?);
            ().serve(transport).await?
        }
        McpServerConfig::InProcess { .. } => {
            return Err(anyhow::anyhow!(
                "in-process servers have no remote connection"
            ));
        }
    };
    Ok(Arc::new(ServerConn { running }))
}

/// Resolve a server's shared backend and its raw tool specs as `(call_name,
/// description, input_schema)`. Remote servers start one persistent connection
/// here; InProcess servers share the surface directly.
async fn server_backend_and_specs(
    server: &McpServerConfig,
) -> anyhow::Result<(McpBackend, Vec<(String, String, Value)>)> {
    match server {
        McpServerConfig::InProcess { server: svc, .. } => Ok((
            McpBackend::InProcess(svc.clone()),
            svc.list_tools()
                .await?
                .into_iter()
                .map(|s| (s.name, s.description, s.input_schema))
                .collect(),
        )),
        remote => {
            let conn = start_remote_server(remote).await?;
            let specs = conn
                .list_tools()
                .await?
                .into_iter()
                .map(|t| {
                    (
                        t.name.to_string(),
                        t.description.map(|d| d.to_string()).unwrap_or_default(),
                        Value::Object((*t.input_schema).clone()),
                    )
                })
                .collect();
            Ok((McpBackend::Remote(conn), specs))
        }
    }
}

/// A single MCP tool. Dispatches through its server's shared backend (one
/// persistent connection), not a per-call re-dial.
struct McpTool {
    backend: McpBackend,
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
        let input_args = match input {
            Value::Object(m) => m,
            _ => serde_json::Map::new(),
        };
        let resp = match &self.backend {
            McpBackend::Remote(conn) => {
                let params = CallToolRequestParams::new(self.call_name.clone())
                    .with_arguments(input_args.into_iter().collect());
                conn.call_tool(params).await?
            }
            McpBackend::InProcess(svc) => {
                return svc
                    .call_tool(&self.call_name, Value::Object(input_args))
                    .await;
            }
        };
        Ok(to_tool_result(resp))
    }
}

fn http_transport_config(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<StreamableHttpClientTransportConfig> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        custom_headers.insert(
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid MCP header name {name:?}: {e}"))?,
            HeaderValue::from_str(value)
                .map_err(|e| anyhow::anyhow!("invalid MCP header value for {name:?}: {e}"))?,
        );
    }
    if !custom_headers.is_empty() {
        config = config.custom_headers(custom_headers);
    }
    Ok(config)
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
        assert!(f.permits("mcp__blackbox__bbox_search"));
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
        let config = McpConfig::from_json(
            r#"{
                "mcpServers": {},
                "tool_placement": {
                    "mcp__blackbox__bbox_knowledge": "in-box",
                    "mcp__blackbox__bbox_hybrid_search": "out-box",
                    "mcp__blackbox__bbox_search": "both"
                }
            }"#,
        )
        .unwrap();
        let placements = config.tool_placement;
        assert_eq!(
            placements.get("mcp__blackbox__bbox_knowledge"),
            Some(&ToolPlacement::InBox)
        );
        assert_eq!(
            placements.get("mcp__blackbox__bbox_hybrid_search"),
            Some(&ToolPlacement::OutBox)
        );
        assert_eq!(
            placements.get("mcp__blackbox__bbox_search"),
            Some(&ToolPlacement::Both)
        );
        assert_eq!(placements.get("mcp__blackbox__unlisted"), None);

        let tools = vec![
            mock_tool("mcp__blackbox__bbox_knowledge"),
            mock_tool("mcp__blackbox__bbox_hybrid_search"),
            mock_tool("mcp__blackbox__bbox_search"),
            mock_tool("mcp__blackbox__unlisted"),
        ];
        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &placements);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(
            in_names,
            vec![
                "mcp__blackbox__bbox_knowledge",
                "mcp__blackbox__bbox_search"
            ]
        );
        assert_eq!(
            out_names,
            vec![
                "mcp__blackbox__bbox_hybrid_search",
                "mcp__blackbox__bbox_search",
                "mcp__blackbox__unlisted"
            ]
        );
    }

    #[test]
    fn cli_json_config_accepts_stdio_entries() {
        let parsed = McpConfig::from_json(
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

        assert_eq!(parsed.servers.len(), 1);
        match &parsed.servers[0] {
            McpServerConfig::Stdio {
                name,
                command,
                args,
                env,
            } => {
                assert_eq!(name, "local_tools");
                assert_eq!(command, "/opt/bin/local-tools");
                assert_eq!(args, &vec!["--scope".to_string(), "probe".to_string()]);
                assert_eq!(
                    env,
                    &BTreeMap::from([("LOCAL_TOOLS_SCOPE".to_string(), "probe".to_string())])
                );
            }
            _ => panic!("expected stdio server"),
        }
    }

    #[test]
    fn cli_json_config_preserves_url_entries() {
        let parsed = McpConfig::from_json(
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

        assert_eq!(parsed.servers.len(), 1);
        match &parsed.servers[0] {
            McpServerConfig::Http {
                name,
                url,
                headers,
                exclude_tools,
            } => {
                assert_eq!(name, "blackbox");
                assert_eq!(url, "http://127.0.0.1:7264/mcp");
                assert!(headers.is_empty());
                assert!(exclude_tools.is_empty());
            }
            _ => panic!("expected http server"),
        }
    }

    #[test]
    fn http_transport_carries_resolved_headers() {
        let config = http_transport_config(
            "http://127.0.0.1:7264/mcp",
            &BTreeMap::from([("X-Auth".to_string(), "token123".to_string())]),
        )
        .unwrap();

        assert_eq!(
            config
                .custom_headers
                .get(&HeaderName::from_static("x-auth")),
            Some(&HeaderValue::from_static("token123"))
        );
    }

    struct FakeSurface;

    #[async_trait]
    impl McpSurface for FakeSurface {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
            Ok(vec![
                McpToolSpec {
                    name: "placed".to_string(),
                    description: "placed".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
                McpToolSpec {
                    name: "default_out".to_string(),
                    description: "default out".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                },
            ])
        }

        async fn call_tool(&self, tool: &str, input: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::Json(serde_json::json!({
                "tool": tool,
                "input": input,
            })))
        }
    }

    #[tokio::test]
    async fn injected_config_loads_servers_and_applies_placement_split() {
        let config = McpConfig {
            servers: vec![McpServerConfig::InProcess {
                name: "sdk".to_string(),
                server: Arc::new(FakeSurface),
            }],
            tool_placement: ToolPlacementMap::from([(
                "mcp__sdk__placed".to_string(),
                ToolPlacement::InBox,
            )]),
        };

        let tools = load_mcp_tools_from_config(&config, &ToolFilter::default()).await;
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__sdk__placed", "mcp__sdk__default_out"]);

        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &config.tool_placement);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(in_names, vec!["mcp__sdk__placed"]);
        assert_eq!(out_names, vec!["mcp__sdk__default_out"]);
    }
}

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
//! Startup is bounded per server. Required failures abort session construction;
//! optional failures publish sanitized readiness. Catalogs are fixed for the
//! session. Dynamic list-change reconciliation is not implemented.

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use http::{HeaderName, HeaderValue};

use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;

use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

mod admission;
mod config;
mod remote;
pub(crate) mod result;
pub use admission::{
    McpLoad, McpServerReadiness, load_mcp_tools, load_mcp_tools_from_config,
    load_mcp_tools_from_config_with_capability_aliases, load_mcp_tools_with_capability_aliases,
};
pub use config::McpServerPolicy;
use remote::ServerConn;

#[derive(Clone)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    pub tool_placement: ToolPlacementMap,
    pub server_policies: BTreeMap<String, McpServerPolicy>,
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

#[derive(Debug, Clone, Default)]
pub struct McpToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub title: Option<String>,
    pub annotations: Option<rmcp::model::ToolAnnotations>,
    pub output_schema: Option<Value>,
}

#[async_trait]
pub trait McpSurface: Send + Sync {
    async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>>;
    /// Native host result, projected into the same MCP envelope as remote tools.
    async fn call_tool(&self, tool: &str, input: Value) -> anyhow::Result<ToolResult>;
}

fn capability_alias(call_name: &str) -> Option<&'static str> {
    match call_name {
        "bbox_corpus_search" => Some("corpus_search"),
        _ => None,
    }
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

/// Read placement through the same strict config validation used for admission.
pub fn parse_tool_placement(mcp_config: Option<&str>) -> anyhow::Result<ToolPlacementMap> {
    mcp_config
        .map(McpConfig::from_json)
        .transpose()
        .map(|config| {
            config
                .map(|config| config.tool_placement)
                .unwrap_or_default()
        })
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
async fn start_remote_server(
    server: &McpServerConfig,
    tool_timeout_ms: u64,
) -> anyhow::Result<Arc<ServerConn>> {
    let running = match server {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).envs(env).kill_on_drop(true);
            let transport = TokioChildProcess::new(cmd.configure(|_| {}))?;
            ().serve(transport).await?
        }
        McpServerConfig::Http { url, headers, .. } => {
            let transport =
                StreamableHttpClientTransport::from_config(http_transport_config(url, headers)?);
            ().serve(transport).await?
        }
        McpServerConfig::Sse { .. } => anyhow::bail!("legacy SSE MCP transport is unsupported"),
        McpServerConfig::InProcess { .. } => {
            return Err(anyhow::anyhow!(
                "in-process servers have no remote connection"
            ));
        }
    };
    Ok(Arc::new(ServerConn::new(
        running,
        server.name().to_owned(),
        tool_timeout_ms,
    )))
}

/// Resolve one persistent backend and retain the server's declared tool metadata.
async fn server_backend_and_specs(
    server: &McpServerConfig,
    tool_timeout_ms: u64,
) -> anyhow::Result<(McpBackend, Vec<McpToolSpec>)> {
    match server {
        McpServerConfig::InProcess { server: svc, .. } => {
            Ok((McpBackend::InProcess(svc.clone()), svc.list_tools().await?))
        }
        remote => {
            let conn = start_remote_server(remote, tool_timeout_ms).await?;
            let specs = conn
                .list_tools()
                .await?
                .into_iter()
                .map(remote_tool_spec)
                .collect();
            Ok((McpBackend::Remote(conn), specs))
        }
    }
}

fn remote_tool_spec(tool: rmcp::model::Tool) -> McpToolSpec {
    McpToolSpec {
        name: tool.name.to_string(),
        description: tool
            .description
            .map(|description| description.to_string())
            .unwrap_or_default(),
        input_schema: Value::Object((*tool.input_schema).clone()),
        title: tool.title,
        annotations: tool.annotations,
        output_schema: tool
            .output_schema
            .map(|schema| Value::Object((*schema).clone())),
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
    output_schema: Value,
    annotations: bro_tools::ToolAnnotations,
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
    fn output_schema(&self) -> Option<Value> {
        Some(self.output_schema.clone())
    }
    fn annotations(&self) -> bro_tools::ToolAnnotations {
        self.annotations
    }
    fn uncertain_outcome(&self) -> Option<String> {
        match &self.backend {
            McpBackend::Remote(connection) => connection.uncertain_outcome(),
            McpBackend::InProcess(_) => None,
        }
    }
    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        match self.call_inner(input, &cx.cancellation).await {
            Ok(r) => r,
            Err(e) => ToolResult::Error(format!("mcp call '{}' failed: {e:#}", self.name)),
        }
    }
}

impl McpTool {
    async fn call_inner(
        &self,
        input: Value,
        cancellation: &tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<ToolResult> {
        // Always send an arguments object (even empty) — some servers reject a
        // missing `arguments` field with -32602.
        let input_args = match input {
            Value::Object(m) => m,
            _ => anyhow::bail!("MCP tool arguments must be an object"),
        };
        match &self.backend {
            McpBackend::Remote(conn) => {
                let params = CallToolRequestParams::new(self.call_name.clone())
                    .with_arguments(input_args.into_iter().collect());
                Ok(conn.call_tool(params, cancellation).await)
            }
            McpBackend::InProcess(svc) => {
                let native = svc
                    .call_tool(&self.call_name, Value::Object(input_args))
                    .await?;
                Ok(result::from_native_result(native))
            }
        }
    }
}

fn http_transport_config(
    url: &str,
    headers: &BTreeMap<String, String>,
) -> anyhow::Result<StreamableHttpClientTransportConfig> {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    let mut custom_headers = HashMap::new();
    for (name, value) in headers {
        let resolved = match value.strip_prefix("$env:") {
            Some(variable) if !variable.is_empty() => std::env::var(variable).map_err(|_| {
                anyhow::anyhow!(
                    "MCP header {name:?} requires missing environment variable {variable:?}"
                )
            })?,
            Some(_) => {
                return Err(anyhow::anyhow!(
                    "MCP header {name:?} has an empty environment reference"
                ));
            }
            None => value.clone(),
        };
        custom_headers.insert(
            HeaderName::from_bytes(name.as_bytes())
                .map_err(|e| anyhow::anyhow!("invalid MCP header name {name:?}: {e}"))?,
            HeaderValue::from_str(&resolved)
                .map_err(|e| anyhow::anyhow!("invalid MCP header value for {name:?}: {e}"))?,
        );
    }
    if !custom_headers.is_empty() {
        config = config.custom_headers(custom_headers);
    }
    Ok(config)
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

    #[test]
    fn http_transport_resolves_header_environment_reference() {
        let expected = std::env::var("PATH").expect("test process PATH");
        let config = http_transport_config(
            "http://127.0.0.1:7264/mcp",
            &BTreeMap::from([("X-Auth".to_string(), "$env:PATH".to_string())]),
        )
        .unwrap();

        assert_eq!(
            config
                .custom_headers
                .get(&HeaderName::from_static("x-auth")),
            Some(&HeaderValue::from_str(&expected).unwrap())
        );
    }

    #[test]
    fn http_transport_refuses_missing_header_environment_reference() {
        let error = http_transport_config(
            "http://127.0.0.1:7264/mcp",
            &BTreeMap::from([(
                "X-Auth".to_string(),
                "$env:BRO_TEST_DEFINITELY_MISSING_HEADER".to_string(),
            )]),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("requires missing environment variable")
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
                    ..Default::default()
                },
                McpToolSpec {
                    name: "default_out".to_string(),
                    description: "default out".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    ..Default::default()
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
            server_policies: Default::default(),
        };

        let tools = load_mcp_tools_from_config(&config, &ToolFilter::default())
            .await
            .unwrap()
            .tools;
        let names: Vec<_> = tools.iter().map(|t| t.name()).collect();
        assert_eq!(names, vec!["mcp__sdk__placed", "mcp__sdk__default_out"]);
        assert!(
            tools
                .iter()
                .all(|tool| tool.description().contains(result::RESULT_GUIDANCE))
        );

        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &config.tool_placement);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(in_names, vec!["mcp__sdk__placed"]);
        assert_eq!(out_names, vec!["mcp__sdk__default_out"]);
    }

    struct CapabilitySurface;

    #[async_trait]
    impl McpSurface for CapabilitySurface {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
            Ok(vec![
                McpToolSpec {
                    name: "bbox_corpus_search".to_string(),
                    description: "corpus".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    ..Default::default()
                },
                McpToolSpec {
                    name: "external_action".to_string(),
                    description: "qualified external capability".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    ..Default::default()
                },
                McpToolSpec {
                    name: "bbox_search".to_string(),
                    description: "full catalog member".to_string(),
                    input_schema: serde_json::json!({"type": "object"}),
                    ..Default::default()
                },
            ])
        }

        async fn call_tool(&self, tool: &str, _input: Value) -> anyhow::Result<ToolResult> {
            Ok(ToolResult::Text(tool.to_string()))
        }
    }

    #[tokio::test]
    async fn capability_aliases_preserve_flat_names_and_complete_qualified_catalog() {
        let config = McpConfig {
            servers: vec![McpServerConfig::InProcess {
                name: "blackbox".to_string(),
                server: Arc::new(CapabilitySurface),
            }],
            tool_placement: ToolPlacementMap::new(),
            server_policies: Default::default(),
        };

        let tools = load_mcp_tools_from_config_with_capability_aliases(
            &config,
            &ToolFilter::default(),
            Some("blackbox"),
        )
        .await
        .unwrap()
        .tools;
        let names: Vec<_> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            vec![
                "corpus_search",
                "mcp__blackbox__bbox_corpus_search",
                "mcp__blackbox__external_action",
                "mcp__blackbox__bbox_search",
            ]
        );

        let flat_only = load_mcp_tools_from_config_with_capability_aliases(
            &config,
            &ToolFilter::from_csv(None, Some("corpus_search")),
            Some("blackbox"),
        )
        .await
        .unwrap()
        .tools;
        assert_eq!(
            flat_only.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["corpus_search"]
        );

        let source_denied = load_mcp_tools_from_config_with_capability_aliases(
            &config,
            &ToolFilter::from_csv(
                Some("mcp__blackbox__bbox_corpus_search"),
                Some("corpus_search"),
            ),
            Some("blackbox"),
        )
        .await
        .unwrap()
        .tools;
        assert!(
            source_denied.is_empty(),
            "a qualified-source deny must also close its flat alias"
        );
    }
}

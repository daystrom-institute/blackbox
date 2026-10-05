//! MCP client: connect to typed or CLI-injected MCP server config and expose
//! their tools as `bro_tools::Tool` impls, merged into the registry alongside
//! the built-in workspace/web tools.
//!
//! Transport + call pattern mirror the daemon's own outbound client: streamable
//! HTTP uses rmcp's reqwest transport, and stdio uses rmcp's `TokioChildProcess`.
//! Connections are **persistent per server**: one connection is started when a
//! server is admitted and Arc-shared by every `McpTool` it produces plus, for
//! servers reachable through the resource helpers, the shared resource state
//! those helpers hold. A resource-only server (no tools) therefore keeps its
//! connection for the whole session instead of being dropped when no
//! `McpTool` retains it; stdio children are reaped via `kill_on_drop(true)`.
//!
//! Lifecycle: every connection uses the `initialize` handshake at 2025-06-18
//! unless `BRO_HARNESS_MCP_HTTP_LIFECYCLE=auto`, which makes HTTP servers
//! probe `server/discover` first and fall back to the handshake when the
//! peer rejects it, serves no sessionless revision, or stays silent. The probe is used only for a server
//! whose `startup_timeout_ms` is at least 20 s, since the SDK waits a fixed
//! 10 s on a silent peer before falling back. Stdio servers always use the
//! handshake. Code that reads a connection must hold for both: a sessionless
//! peer has no session id, and its results carry a `resultType`.
//!
//! The variable is read from the harness process environment. The daemon
//! removes it from the environment a dispatched worker inherits, so it
//! reaches a worker only through an explicit account or per-dispatch env.
//!
//! Startup is bounded per server. Required failures abort session construction;
//! optional failures publish sanitized readiness. Catalogs are fixed for the
//! session. Dynamic list-change reconciliation is not implemented.

use async_trait::async_trait;
use bro_tools::{Tool, ToolCx, ToolResult};
use http::{HeaderName, HeaderValue};

use rmcp::model::{CallToolRequestParams, ClientConfig, ProtocolVersion};
use rmcp::service::{ClientLifecycleMode, ClientServiceExt};

use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

mod admission;
mod config;
mod remote;
mod resources;
pub(crate) mod result;
pub use admission::{
    McpLoad, McpServerReadiness, load_mcp_tools, load_mcp_tools_from_config,
    load_mcp_tools_from_config_with_capability_aliases, load_mcp_tools_with_capability_aliases,
};
pub use config::McpServerPolicy;
use remote::ServerConn;
pub use resources::ResourcePage;

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

    /// Whether this surface exposes the MCP resources capability. Surfaces
    /// that implement the resource methods below must also declare support
    /// here; the resource helpers fail closed on an undeclared capability.
    fn resources_supported(&self) -> bool {
        false
    }
    /// One page of resources (`resources/list`). Items are raw MCP resource
    /// JSON objects (`uri`, `name`, `description`, `mimeType`, ...).
    async fn list_resources(&self, _cursor: Option<String>) -> anyhow::Result<ResourcePage> {
        anyhow::bail!("in-process MCP server does not expose resources")
    }
    /// One page of resource templates (`resources/templates/list`). Items are
    /// raw MCP resource template JSON objects (`uriTemplate`, `name`, ...).
    async fn list_resource_templates(
        &self,
        _cursor: Option<String>,
    ) -> anyhow::Result<ResourcePage> {
        anyhow::bail!("in-process MCP server does not expose resources")
    }
    /// Read one resource (`resources/read`). Contents are raw MCP content
    /// JSON objects (`uri`, `text`/`blob`, `mimeType`, ...).
    async fn read_resource(&self, _uri: &str) -> anyhow::Result<Vec<Value>> {
        anyhow::bail!("in-process MCP server does not expose resources")
    }
}

fn capability_alias(call_name: &str) -> Option<&'static str> {
    match call_name {
        "bbox_hybrid_search" => Some("corpus_search"),
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

/// Client identity for the `initialize` handshake. The requested revision
/// must be one that has a handshake.
fn legacy_client_config() -> ClientConfig {
    ClientConfig::default().with_protocol_version(ProtocolVersion::V_2025_06_18)
}

/// Selects the lifecycle for HTTP MCP servers. Unset or any other value
/// means the handshake; `auto` probes `server/discover` first.
const HTTP_LIFECYCLE_ENV: &str = "BRO_HARNESS_MCP_HTTP_LIFECYCLE";

/// The SDK waits this long for a discovery answer before `auto` falls back
/// to the handshake, and offers no way to shorten it.
const AUTO_DISCOVERY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// The smallest startup budget under which `auto` is used: the discovery
/// wait, and as long again for the handshake that follows a silent peer.
const AUTO_MIN_STARTUP_BUDGET: std::time::Duration = AUTO_DISCOVERY_WAIT.saturating_mul(2);

/// How a connection to an HTTP MCP server is established. `auto` is honored
/// only when the server's startup budget leaves room for a peer that never
/// answers discovery; under a tighter budget the connection uses the
/// handshake, so a server that connects today is never timed out by the
/// probe. Stdio servers always use the handshake: a child that ignores an
/// unknown method would hold startup for the whole discovery wait.
fn http_lifecycle(
    selected: Option<&str>,
    startup_budget: std::time::Duration,
) -> ClientLifecycleMode {
    match selected.map(str::trim) {
        Some(value)
            if value.eq_ignore_ascii_case("auto") && startup_budget >= AUTO_MIN_STARTUP_BUDGET =>
        {
            ClientLifecycleMode::Auto {
                preferred_versions: vec![ProtocolVersion::V_2026_07_28],
                legacy_version: Some(ProtocolVersion::V_2025_06_18),
            }
        }
        _ => ClientLifecycleMode::Initialize,
    }
}

/// Serve one connection under `lifecycle`, connecting again with the
/// handshake when discovery finds no common sessionless revision. A peer that
/// knows `server/discover` but serves only handshake revisions refuses the
/// probe as an unsupported version; the SDK reports that as no compatible
/// version instead of falling back, and the refused exchange has used up the
/// transport, so the handshake needs a new one.
async fn serve_with_handshake_fallback<T, E, A>(
    mut connect: impl FnMut() -> anyhow::Result<T>,
    lifecycle: ClientLifecycleMode,
) -> anyhow::Result<rmcp::service::RunningService<rmcp::RoleClient, ClientConfig>>
where
    T: rmcp::transport::IntoTransport<rmcp::RoleClient, E, A>,
    E: std::error::Error + Send + Sync + 'static,
{
    let probes = lifecycle != ClientLifecycleMode::Initialize;
    match legacy_client_config()
        .serve_with_lifecycle(connect()?, lifecycle)
        .await
    {
        Err(rmcp::service::ClientInitializeError::NoCompatibleProtocolVersion { .. }) if probes => {
            Ok(legacy_client_config()
                .serve_with_lifecycle(connect()?, ClientLifecycleMode::Initialize)
                .await?)
        }
        served => Ok(served?),
    }
}

/// Start one persistent connection to a remote (stdio/http/sse) MCP server.
/// InProcess servers have no rmcp connection and are handled by the caller.
async fn start_remote_server(
    server: &McpServerConfig,
    tool_timeout_ms: u64,
    startup_timeout_ms: u64,
) -> anyhow::Result<Arc<ServerConn>> {
    let running = match server {
        McpServerConfig::Stdio {
            command, args, env, ..
        } => {
            let mut cmd = tokio::process::Command::new(command);
            cmd.args(args).envs(env).kill_on_drop(true);
            let transport = TokioChildProcess::new(cmd.configure(|_| {}))?;
            legacy_client_config()
                .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
                .await?
        }
        McpServerConfig::Http { url, headers, .. } => {
            let lifecycle = http_lifecycle(
                std::env::var(HTTP_LIFECYCLE_ENV).ok().as_deref(),
                std::time::Duration::from_millis(startup_timeout_ms),
            );
            serve_with_handshake_fallback(
                || {
                    Ok(StreamableHttpClientTransport::from_config(
                        http_transport_config(url, headers)?,
                    ))
                },
                lifecycle,
            )
            .await?
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
    startup_timeout_ms: u64,
) -> anyhow::Result<(McpBackend, Vec<McpToolSpec>)> {
    match server {
        McpServerConfig::InProcess { server: svc, .. } => {
            Ok((McpBackend::InProcess(svc.clone()), svc.list_tools().await?))
        }
        remote => {
            let conn = start_remote_server(remote, tool_timeout_ms, startup_timeout_ms).await?;
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

    const ROOMY: std::time::Duration = std::time::Duration::from_secs(30);

    #[test]
    fn the_http_lifecycle_is_the_handshake_unless_auto_is_selected() {
        for legacy in [None, Some(""), Some("legacy"), Some("discover"), Some("1")] {
            assert_eq!(
                http_lifecycle(legacy, ROOMY),
                ClientLifecycleMode::Initialize,
                "{legacy:?}"
            );
        }
        let auto = ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_06_18),
        };
        for selected in ["auto", " AUTO "] {
            assert_eq!(http_lifecycle(Some(selected), ROOMY), auto);
        }
        // The probe needs room for a silent peer inside the startup budget.
        assert_eq!(http_lifecycle(Some("auto"), AUTO_MIN_STARTUP_BUDGET), auto);
        for tight in [
            AUTO_MIN_STARTUP_BUDGET - std::time::Duration::from_millis(1),
            AUTO_DISCOVERY_WAIT,
            std::time::Duration::from_millis(1),
        ] {
            assert_eq!(
                http_lifecycle(Some("auto"), tight),
                ClientLifecycleMode::Initialize,
                "{tight:?}"
            );
        }
    }

    #[derive(Clone, Copy)]
    enum Discovery {
        Answered,
        Rejected,
        /// Known method, but no sessionless revision is served.
        Unsupported,
        Ignored,
    }

    /// Connects with `auto` selected, under `startup_budget` and the same
    /// timeout admission applies, to a line-delimited peer that answers,
    /// rejects or ignores discovery; lists tools; and returns every request
    /// the peer saw.
    async fn auto_lifecycle_requests(
        discovery: Discovery,
        startup_budget: std::time::Duration,
    ) -> Vec<Value> {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let peers = Arc::new(std::sync::Mutex::new(Vec::new()));
        // Each connection gets its own peer; all of them record into one log.
        let connect = {
            let requests = requests.clone();
            let peers = peers.clone();
            move || {
                let (client, server) = tokio::io::duplex(16 * 1024);
                let captured = requests.clone();
                peers.lock().unwrap().push(tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(server);
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: Value = serde_json::from_str(&line).unwrap();
                        captured.lock().unwrap().push(request.clone());
                        let id = request["id"].clone();
                        let reply = match (request["method"].as_str().unwrap(), discovery) {
                            ("server/discover", Discovery::Answered) => serde_json::json!({
                                "jsonrpc":"2.0","id":id,"result":{
                                    "resultType":"complete",
                                    "supportedVersions":["2025-06-18","2026-07-28"],
                                    "capabilities":{"tools":{}},
                                    "ttlMs":1000,"cacheScope":"private"}}),
                            ("server/discover", Discovery::Rejected) => serde_json::json!({
                                "jsonrpc":"2.0","id":id,
                                "error":{"code":-32601,"message":"Method not found"}}),
                            ("server/discover", Discovery::Unsupported) => serde_json::json!({
                                "jsonrpc":"2.0","id":id,
                                "error":{"code":-32022,"message":"Unsupported protocol version",
                                    "data":{"supported":["2024-11-05","2025-03-26","2025-06-18"],
                                        "requested":"2026-07-28"}}}),
                            ("server/discover", Discovery::Ignored) => continue,
                            ("initialize", _) => serde_json::json!({
                                "jsonrpc":"2.0","id":id,"result":{
                                    "protocolVersion":request["params"]["protocolVersion"],
                                    "capabilities":{"tools":{}},
                                    "serverInfo":{"name":"fixture","version":"1"}}}),
                            ("tools/list", _) => serde_json::json!({
                                "jsonrpc":"2.0","id":id,"result":{
                                    "resultType":"complete","tools":[],
                                    "ttlMs":1000,"cacheScope":"private"}}),
                            ("notifications/initialized", _) => continue,
                            (method, _) => panic!("unexpected fixture method {method}"),
                        };
                        if write
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                }));
                Ok(client)
            }
        };
        let conn = tokio::time::timeout(startup_budget, async {
            let running = serve_with_handshake_fallback(
                connect,
                http_lifecycle(Some("auto"), startup_budget),
            )
            .await
            .unwrap();
            let conn = remote::ServerConn::new(running, "fixture".into(), 5_000);
            assert!(conn.list_tools().await.unwrap().is_empty());
            conn
        })
        .await
        .expect("the connection starts inside its startup budget");
        drop(conn);
        for peer in peers.lock().unwrap().drain(..) {
            peer.abort();
        }
        let seen = requests.lock().unwrap().clone();
        seen
    }

    fn methods(requests: &[Value]) -> Vec<&str> {
        requests
            .iter()
            .map(|request| request["method"].as_str().unwrap())
            .collect()
    }

    const HANDSHAKE: [&str; 3] = ["initialize", "notifications/initialized", "tools/list"];

    #[tokio::test]
    async fn auto_uses_the_sessionless_lifecycle_when_the_peer_answers_discovery() {
        let requests = auto_lifecycle_requests(Discovery::Answered, ROOMY).await;
        assert_eq!(methods(&requests), ["server/discover", "tools/list"]);
        assert_eq!(
            requests[1]["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"], "2026-07-28",
            "{}",
            requests[1]
        );
    }

    #[tokio::test]
    async fn auto_falls_back_to_the_handshake_when_the_peer_rejects_discovery() {
        let requests = auto_lifecycle_requests(Discovery::Rejected, ROOMY).await;
        assert_eq!(methods(&requests)[0], "server/discover");
        assert_eq!(methods(&requests)[1..], HANDSHAKE);
        assert_eq!(requests[1]["params"]["protocolVersion"], "2025-06-18");
        assert!(
            requests[3]["params"]["_meta"]
                .get("io.modelcontextprotocol/protocolVersion")
                .is_none(),
            "{}",
            requests[3]
        );
    }

    /// A peer that knows discovery but serves only handshake revisions
    /// refuses the probe as an unsupported version. That is this daemon with
    /// its modern lifecycle off, and it must still connect.
    #[tokio::test]
    async fn auto_reconnects_with_the_handshake_when_no_sessionless_revision_is_served() {
        let requests = auto_lifecycle_requests(Discovery::Unsupported, ROOMY).await;
        assert_eq!(methods(&requests)[0], "server/discover");
        assert_eq!(methods(&requests)[1..], HANDSHAKE);
        assert_eq!(requests[1]["params"]["protocolVersion"], "2025-06-18");
    }

    /// Connects under `lifecycle` to peers that refuse discovery as an
    /// unsupported version and fail the handshake, and returns how many
    /// connections were made before the error came back.
    async fn connections_before_a_failed_handshake(lifecycle: ClientLifecycleMode) -> usize {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let connections = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let connect = {
            let connections = connections.clone();
            move || {
                connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let (client, server) = tokio::io::duplex(16 * 1024);
                tokio::spawn(async move {
                    let (read, mut write) = tokio::io::split(server);
                    let mut lines = tokio::io::BufReader::new(read).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        let request: Value = serde_json::from_str(&line).unwrap();
                        let error = match request["method"].as_str().unwrap() {
                            "server/discover" => serde_json::json!({
                                "code":-32022,"message":"Unsupported protocol version",
                                "data":{"supported":["2025-06-18"],"requested":"2026-07-28"}}),
                            _ => serde_json::json!({"code":-32603,"message":"handshake refused"}),
                        };
                        let reply =
                            serde_json::json!({"jsonrpc":"2.0","id":request["id"],"error":error});
                        if write
                            .write_all(format!("{reply}\n").as_bytes())
                            .await
                            .is_err()
                        {
                            return;
                        }
                    }
                });
                Ok(client)
            }
        };
        let served = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            serve_with_handshake_fallback(connect, lifecycle),
        )
        .await
        .expect("a refused connection fails promptly");
        assert!(served.is_err(), "a refused handshake is an error");
        connections.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The second connection exists only for a probe that found no common
    /// revision. A handshake that fails is never retried, and when the
    /// handshake after such a probe fails too, that failure is the result.
    #[tokio::test]
    async fn only_a_probe_without_a_common_revision_earns_one_more_connection() {
        assert_eq!(
            connections_before_a_failed_handshake(ClientLifecycleMode::Initialize).await,
            1
        );
        assert_eq!(
            connections_before_a_failed_handshake(http_lifecycle(Some("auto"), ROOMY)).await,
            2
        );
    }

    /// A peer that never answers discovery costs the SDK's fixed wait, and
    /// the handshake that follows still fits a budget that admits the probe.
    #[tokio::test(start_paused = true)]
    async fn auto_falls_back_inside_a_roomy_budget_when_the_peer_ignores_discovery() {
        let started = tokio::time::Instant::now();
        let requests = auto_lifecycle_requests(Discovery::Ignored, AUTO_MIN_STARTUP_BUDGET).await;
        assert_eq!(methods(&requests)[0], "server/discover");
        assert_eq!(methods(&requests)[1..], HANDSHAKE);
        assert!(started.elapsed() >= AUTO_DISCOVERY_WAIT);
    }

    /// Under a budget too tight for the probe the same peer is never probed,
    /// so it connects as it does without the flag instead of timing out.
    #[tokio::test(start_paused = true)]
    async fn a_tight_budget_skips_the_probe_for_a_peer_that_ignores_discovery() {
        let started = tokio::time::Instant::now();
        let requests = auto_lifecycle_requests(Discovery::Ignored, AUTO_DISCOVERY_WAIT).await;
        assert_eq!(methods(&requests), HANDSHAKE);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

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
        assert!(f.permits("mcp__blackbox__bbox_context"));
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
                    "mcp__blackbox__bbox_context": "both"
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
            placements.get("mcp__blackbox__bbox_context"),
            Some(&ToolPlacement::Both)
        );
        assert_eq!(placements.get("mcp__blackbox__unlisted"), None);

        let tools = vec![
            mock_tool("mcp__blackbox__bbox_knowledge"),
            mock_tool("mcp__blackbox__bbox_hybrid_search"),
            mock_tool("mcp__blackbox__bbox_context"),
            mock_tool("mcp__blackbox__unlisted"),
        ];
        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &placements);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(
            in_names,
            vec![
                "mcp__blackbox__bbox_knowledge",
                "mcp__blackbox__bbox_context"
            ]
        );
        assert_eq!(
            out_names,
            vec![
                "mcp__blackbox__bbox_hybrid_search",
                "mcp__blackbox__bbox_context",
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
        assert_eq!(
            names,
            vec![
                "mcp__sdk__placed",
                "mcp__sdk__default_out",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );
        assert!(
            tools
                .iter()
                .take(2)
                .all(|tool| tool.description().contains(result::RESULT_GUIDANCE))
        );

        let (in_box, out_box) = split_mcp_tools_by_placement(&tools, &config.tool_placement);
        let in_names: Vec<_> = in_box.iter().map(|t| t.name()).collect();
        let out_names: Vec<_> = out_box.iter().map(|t| t.name()).collect();
        assert_eq!(in_names, vec!["mcp__sdk__placed"]);
        assert_eq!(
            out_names,
            vec![
                "mcp__sdk__default_out",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource"
            ]
        );
    }

    struct CapabilitySurface;

    #[async_trait]
    impl McpSurface for CapabilitySurface {
        async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
            Ok(vec![
                McpToolSpec {
                    name: "bbox_hybrid_search".to_string(),
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
                    name: "bbox_context".to_string(),
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
                "mcp__blackbox__bbox_hybrid_search",
                "mcp__blackbox__external_action",
                "mcp__blackbox__bbox_context",
                "list_mcp_resources",
                "list_mcp_resource_templates",
                "read_mcp_resource",
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
                Some("mcp__blackbox__bbox_hybrid_search"),
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

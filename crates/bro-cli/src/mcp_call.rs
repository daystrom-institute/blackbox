use anyhow::{Context, bail};
use bbox_corpus_core::blame_transport::{
    OPERATOR_BLAME_REPO_ID_HEADER, OPERATOR_BLAME_ROOT_RELPATH_HEADER,
    OPERATOR_BLAME_WORKSPACE_ID_HEADER,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_provenance::{
    OPERATOR_PROVENANCE_REPO_ID_HEADER, OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
};
use bro_rpc::ServiceToken;
use clap::{Args, Subcommand};
use reqwest::header::{ACCEPT, CONTENT_TYPE, HeaderMap};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::fmt;
use std::path::Path;

const MCP_PROTOCOL_VERSION: &str = "2025-03-26";
const MCP_SESSION_ID: &str = "mcp-session-id";

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  call <tool_name> <json-args>    call one MCP tool on the local daemon"
)]
pub(crate) struct McpArgs {
    #[command(subcommand)]
    command: McpCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Call one MCP tool on the running blackboxd
    Call(McpCallArgs),
}

#[derive(Debug, Args)]
struct McpCallArgs {
    /// Tool name, e.g. bbox_stats
    #[arg(value_name = "TOOL_NAME")]
    tool_name: String,
    /// JSON object passed as the tool arguments
    #[arg(value_name = "JSON_ARGS")]
    json_args: String,
    /// Daemon base URL. Defaults to the origin of $BLACKBOX_MCP_URL, else
    /// config [client].daemon_url, else http://127.0.0.1:<[daemon].port>.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// MCP surface name (`/mcp?surface=<name>`); default: the daemon's
    /// anonymous `default` surface
    #[arg(long, value_name = "SURFACE")]
    surface: Option<String>,
}

pub(crate) async fn run(args: McpArgs) -> anyhow::Result<()> {
    match args.command {
        McpCommand::Call(call_args) => call(call_args).await,
    }
}

async fn call(args: McpCallArgs) -> anyhow::Result<()> {
    let arguments = parse_arguments(&args.json_args)?;
    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let mut client = match args.surface.as_deref() {
        Some(surface) => McpClient::connect_surface(&base_url, surface).await?,
        None => McpClient::connect(&base_url, None).await?,
    };
    let call_response = client
        .call_tool_response(&args.tool_name, arguments)
        .await?;
    print_tool_response(&call_response)
}

pub(crate) struct McpClient {
    client: reqwest::Client,
    mcp_url: String,
    session_id: Option<String>,
    next_id: u64,
}

#[derive(Debug)]
struct McpToolError {
    message: String,
}

impl fmt::Display for McpToolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "MCP tool returned an error result: {}",
            self.message
        )
    }
}

impl std::error::Error for McpToolError {}

pub(crate) fn tool_error_has_code(error: &anyhow::Error, code: &str) -> bool {
    error
        .downcast_ref::<McpToolError>()
        .is_some_and(|tool_error| {
            let message = tool_error.message.trim();
            let code_end = message
                .find(|character: char| character == ':' || character.is_ascii_whitespace())
                .unwrap_or(message.len());
            &message[..code_end] == code
        })
}

impl McpClient {
    pub(crate) async fn connect(
        base_url: &str,
        project_root: Option<&Path>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_initialization_headers(base_url, project_root, None, HeaderMap::new())
            .await
    }

    /// Connect to a named MCP surface (`/mcp?surface=<name>`). The daemon
    /// projects a filtered tool catalog per surface; the anonymous `default`
    /// surface hides operator tools such as `bbox_render`.
    pub(crate) async fn connect_surface(base_url: &str, surface: &str) -> anyhow::Result<Self> {
        Self::connect_with_initialization_headers(base_url, None, Some(surface), HeaderMap::new())
            .await
    }

    pub(crate) async fn connect_with_operator_blame(
        base_url: &str,
        token: &ServiceToken,
        scope: &PublishedScope,
        workspace_id: &str,
    ) -> anyhow::Result<Self> {
        validate_credentialed_base_url(base_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.expose_secret())
                .parse()
                .context("encoding operator blame authorization")?,
        );
        headers.insert(
            OPERATOR_BLAME_REPO_ID_HEADER,
            scope
                .repo_id()
                .parse()
                .context("encoding operator blame repo id")?,
        );
        headers.insert(
            OPERATOR_BLAME_ROOT_RELPATH_HEADER,
            scope
                .bbox_root_relpath()
                .parse()
                .context("encoding operator blame root relative path")?,
        );
        headers.insert(
            OPERATOR_BLAME_WORKSPACE_ID_HEADER,
            workspace_id
                .parse()
                .context("encoding operator blame workspace id")?,
        );
        Self::connect_with_initialization_headers(base_url, None, None, headers).await
    }

    pub(crate) async fn connect_with_operator_provenance(
        base_url: &str,
        token: &ServiceToken,
        scope: &PublishedScope,
    ) -> anyhow::Result<Self> {
        validate_credentialed_base_url(base_url)?;
        let mut headers = HeaderMap::new();
        headers.insert(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", token.expose_secret())
                .parse()
                .context("encoding operator provenance authorization")?,
        );
        headers.insert(
            OPERATOR_PROVENANCE_REPO_ID_HEADER,
            scope
                .repo_id()
                .parse()
                .context("encoding operator provenance repo id")?,
        );
        headers.insert(
            OPERATOR_PROVENANCE_ROOT_RELPATH_HEADER,
            scope
                .bbox_root_relpath()
                .parse()
                .context("encoding operator provenance root relative path")?,
        );
        Self::connect_with_initialization_headers(base_url, None, None, headers).await
    }

    async fn connect_with_initialization_headers(
        base_url: &str,
        project_root: Option<&Path>,
        surface: Option<&str>,
        initialization_headers: HeaderMap,
    ) -> anyhow::Result<Self> {
        let raw_url = format!("{}/mcp", base_url.trim_end_matches('/'));
        let mut mcp_url = reqwest::Url::parse(&raw_url)
            .with_context(|| format!("parsing daemon MCP URL {raw_url}"))?;
        if let Some(root) = project_root {
            let root = root.to_str().context("project root is not valid UTF-8")?;
            mcp_url.query_pairs_mut().append_pair("project", root);
        }
        if let Some(surface) = surface.filter(|s| !s.is_empty()) {
            mcp_url.query_pairs_mut().append_pair("surface", surface);
        }
        let mcp_url = mcp_url.to_string();
        let client = if initialization_headers.is_empty() {
            reqwest::Client::new()
        } else {
            reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .context("building credentialed MCP client")?
        };

        let initialize = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {
                    "name": "bro-cli",
                    "version": env!("CARGO_PKG_VERSION"),
                },
            },
        });
        let (init_response, session_id) = post_json_rpc(
            &client,
            &mcp_url,
            None,
            &initialize,
            Some(&initialization_headers),
        )
        .await?;
        ensure_json_rpc_response_id(&init_response, 1, "initialize")?;
        ensure_json_rpc_success(&init_response, "initialize")?;

        Ok(Self {
            client,
            mcp_url,
            session_id,
            next_id: 2,
        })
    }

    pub(crate) async fn call_tool_json<T: DeserializeOwned>(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<T> {
        let response = self.call_tool_response(tool_name, arguments).await?;
        let value = tool_response_json(&response)?;
        serde_json::from_value(value)
            .with_context(|| format!("decoding {tool_name} response payload"))
    }

    async fn call_tool_response(
        &mut self,
        tool_name: &str,
        arguments: Value,
    ) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let tool_call = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": tool_name,
                "arguments": arguments,
            },
        });
        let (response, _) = post_json_rpc(
            &self.client,
            &self.mcp_url,
            self.session_id.as_deref(),
            &tool_call,
            None,
        )
        .await?;
        ensure_json_rpc_response_id(&response, id, "tools/call")?;
        Ok(response)
    }
}

fn validate_credentialed_base_url(base_url: &str) -> anyhow::Result<()> {
    let url = reqwest::Url::parse(base_url).context("parsing credentialed daemon URL")?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "::1"));
    if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
        bail!("credentialed daemon URL must use HTTPS unless it is loopback HTTP");
    }
    Ok(())
}

/// The daemon base URL when no `--daemon-url` is given: `BLACKBOX_MCP_URL`'s
/// origin, else config `[client].daemon_url`, else loopback on the configured
/// port. See `bro_fleet_client::daemon_url`.
pub(crate) fn default_base_url() -> String {
    bro_fleet_client::daemon_url()
}

fn parse_arguments(raw: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(raw).context("parsing JSON_ARGS as JSON")?;
    if !value.is_object() {
        bail!("JSON_ARGS must be a JSON object");
    }
    Ok(value)
}

async fn post_json_rpc(
    client: &reqwest::Client,
    url: &str,
    session_id: Option<&str>,
    body: &Value,
    extra_headers: Option<&HeaderMap>,
) -> anyhow::Result<(Value, Option<String>)> {
    let mut request = client
        .post(url)
        .header(ACCEPT, "application/json, text/event-stream")
        .header(CONTENT_TYPE, "application/json")
        .header("Mcp-Protocol-Version", MCP_PROTOCOL_VERSION)
        .json(body);
    if let Some(session_id) = session_id {
        request = request.header("Mcp-Session-Id", session_id);
    }
    if let Some(headers) = extra_headers {
        request = request.headers(headers.clone());
    }

    let response = request
        .send()
        .await
        .with_context(|| format!("POST {url}"))?;
    let status = response.status();
    let session_id = response_session_id(response.headers());
    let text = response.text().await.context("reading MCP response body")?;
    if !status.is_success() {
        bail!("MCP HTTP request failed with {status}: {text}");
    }
    let value = decode_json_response(&text)?;
    Ok((value, session_id))
}

fn response_session_id(headers: &HeaderMap) -> Option<String> {
    headers
        .get(MCP_SESSION_ID)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn decode_json_response(text: &str) -> anyhow::Result<Value> {
    if let Ok(value) = serde_json::from_str(text) {
        return Ok(value);
    }

    for event in text.split("\n\n").filter(|event| event.contains("data:")) {
        let data = event
            .lines()
            .filter_map(|line| line.strip_prefix("data:"))
            .map(str::trim_start)
            .collect::<Vec<_>>()
            .join("\n");
        if data.trim().is_empty() {
            continue;
        }
        if let Ok(value) = serde_json::from_str(&data) {
            return Ok(value);
        }
    }

    bail!("MCP response was not JSON: {text}")
}

fn ensure_json_rpc_success(value: &Value, method: &str) -> anyhow::Result<()> {
    if let Some(error) = value.get("error") {
        bail!("MCP {method} failed: {}", pretty_json(error)?);
    }
    if value.get("result").is_none() {
        bail!(
            "MCP {method} response had no result: {}",
            pretty_json(value)?
        );
    }
    Ok(())
}

fn ensure_json_rpc_response_id(value: &Value, expected: u64, method: &str) -> anyhow::Result<()> {
    let observed = value
        .get("id")
        .and_then(Value::as_u64)
        .with_context(|| format!("MCP {method} response had no numeric id"))?;
    if observed != expected {
        bail!("MCP {method} response id mismatch: expected {expected}, observed {observed}");
    }
    Ok(())
}

fn tool_response_json(value: &Value) -> anyhow::Result<Value> {
    if let Some(error) = value.get("error") {
        bail!("MCP tools/call failed: {}", pretty_json(error)?);
    }
    let result = value
        .get("result")
        .context("MCP tools/call response had no result")?;
    let text = result
        .get("content")
        .and_then(Value::as_array)
        .and_then(|content| {
            content.iter().find_map(|item| {
                (item.get("type").and_then(Value::as_str) == Some("text"))
                    .then(|| item.get("text").and_then(Value::as_str))
                    .flatten()
            })
        });
    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Err(McpToolError {
            message: text.unwrap_or("missing error text").to_string(),
        }
        .into());
    }
    let text = text.context("MCP tool response had no text content")?;
    serde_json::from_str(text).context("parsing MCP tool text as JSON")
}

fn print_tool_response(value: &Value) -> anyhow::Result<()> {
    if let Some(error) = value.get("error") {
        eprintln!("{}", pretty_json(error)?);
        bail!("MCP tools/call failed");
    }

    let result = value
        .get("result")
        .context("MCP tools/call response had no result")?;
    let printed_content = print_content(result)?;
    if !printed_content {
        println!("{}", pretty_json(result)?);
    }

    if result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        bail!("MCP tool returned an error result");
    }
    Ok(())
}

fn print_content(result: &Value) -> anyhow::Result<bool> {
    let Some(content) = result.get("content").and_then(Value::as_array) else {
        return Ok(false);
    };

    let mut printed = false;
    for item in content {
        if item.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                print_text(text);
                printed = true;
                continue;
            }
        }
        println!("{}", pretty_json(item)?);
        printed = true;
    }
    Ok(printed)
}

fn print_text(text: &str) {
    if text.ends_with('\n') {
        print!("{text}");
    } else {
        println!("{text}");
    }
}

fn pretty_json(value: &Value) -> anyhow::Result<String> {
    serde_json::to_string_pretty(value).context("formatting MCP JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_sse_json_response() {
        let value = decode_json_response(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
        )
        .unwrap();
        assert_eq!(value["id"], 1);
    }

    #[test]
    fn rejects_non_object_arguments() {
        let err = parse_arguments("[]").unwrap_err().to_string();
        assert!(err.contains("JSON_ARGS must be a JSON object"));
    }

    #[test]
    fn credentialed_mcp_requires_https_or_loopback() {
        assert!(validate_credentialed_base_url("https://corpus.example").is_ok());
        assert!(validate_credentialed_base_url("http://127.0.0.1:7264").is_ok());
        assert!(validate_credentialed_base_url("http://localhost:7264").is_ok());
        assert!(validate_credentialed_base_url("http://192.0.2.10:7264").is_err());
    }

    #[test]
    fn extracts_json_tool_payload() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [{"type": "text", "text": "{\"value\":7}"}],
                "isError": false,
            },
        });
        assert_eq!(tool_response_json(&response).unwrap()["value"], 7);
    }

    #[test]
    fn preserves_structured_tool_error_code() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "result": {
                "content": [{"type": "text", "text": "error.stale_generation: retry"}],
                "isError": true,
            },
        });
        let error = tool_response_json(&response).unwrap_err();
        assert!(tool_error_has_code(&error, "error.stale_generation"));
        assert!(!tool_error_has_code(&error, "stale_generation"));
        assert!(!tool_error_has_code(&error, "error.stale"));
    }

    #[test]
    fn rejects_missing_or_mismatched_json_rpc_response_ids() {
        let missing = json!({"jsonrpc": "2.0", "result": {}});
        assert!(
            ensure_json_rpc_response_id(&missing, 7, "tools/call")
                .unwrap_err()
                .to_string()
                .contains("had no numeric id")
        );

        let mismatched = json!({"jsonrpc": "2.0", "id": 8, "result": {}});
        let error = ensure_json_rpc_response_id(&mismatched, 7, "tools/call")
            .unwrap_err()
            .to_string();
        assert!(error.contains("expected 7, observed 8"));
    }
}

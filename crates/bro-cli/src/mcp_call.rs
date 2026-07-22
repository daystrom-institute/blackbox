use anyhow::{Context, bail};
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
    /// Daemon base URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
}

pub(crate) async fn run(args: McpArgs) -> anyhow::Result<()> {
    match args.command {
        McpCommand::Call(call_args) => call(call_args).await,
    }
}

async fn call(args: McpCallArgs) -> anyhow::Result<()> {
    let arguments = parse_arguments(&args.json_args)?;
    let base_url = args.daemon_url.unwrap_or_else(default_base_url);
    let mut client = McpClient::connect(&base_url, None).await?;
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
        .is_some_and(|tool_error| tool_error.message.contains(code))
}

impl McpClient {
    pub(crate) async fn connect(
        base_url: &str,
        project_root: Option<&Path>,
    ) -> anyhow::Result<Self> {
        let raw_url = format!("{}/mcp", base_url.trim_end_matches('/'));
        let mut mcp_url = reqwest::Url::parse(&raw_url)
            .with_context(|| format!("parsing daemon MCP URL {raw_url}"))?;
        if let Some(root) = project_root {
            let root = root.to_str().context("project root is not valid UTF-8")?;
            mcp_url.query_pairs_mut().append_pair("project", root);
        }
        let mcp_url = mcp_url.to_string();
        let client = reqwest::Client::new();

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
        let (init_response, session_id) =
            post_json_rpc(&client, &mcp_url, None, &initialize).await?;
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
        )
        .await?;
        Ok(response)
    }
}

pub(crate) fn default_base_url() -> String {
    format!("http://127.0.0.1:{}", bro_fleet_client::daemon_port())
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
    }
}

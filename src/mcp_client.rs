//! Outbound MCP client used by the workflow `mcp_call` op.
//!
//! Two transports today, both lifted directly from rmcp 1.4:
//!   - **Stdio** — spawn the configured child process, talk JSON-RPC
//!     over stdin/stdout. Used for biofilter (`node launch.mjs`) and
//!     any other locally-installed stdio MCP server.
//!   - **Streamable HTTP** — `StreamableHttpClientTransport::from_uri`,
//!     used for the blackbox-self call path (`http://127.0.0.1:7264/mcp`).
//!
//! The op resolves a server name through the existing `McpStore`
//! (global `~/.bro/mcp.json` + project overlay) so the same config
//! that drives dispatched-bro MCP also drives engine-level tool
//! invocations. No separate registry — one source of truth.
//!
//! Returned content is normalized into a `serde_json::Value` for the
//! engine: `structured_content` first if the server provided it,
//! otherwise the concatenation of text-content blocks (parsed as JSON
//! when it round-trips). Tool-error results (`is_error: true`) are
//! surfaced as `Err` so the op's `on_failure` handling fires.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, CallToolResult, Content, RawContent};
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use serde_json::Value;
use tokio::time::timeout;

use crate::orchestration::mcp::{McpServerConfig, McpStore, global_store_path, project_store_path};

/// Test-only override for `call_tool`. Workflow tests that walk shipped
/// arcs hit `mcp_call` nodes which would otherwise resolve the OPERATOR'S
/// global MCP registry and call a live daemon — a test-isolation violation
/// (and a rustls "No provider set" panic in processes that never ran
/// daemon startup). Tests install a hook that answers the call in-process;
/// the RAII guard uninstalls on drop.
#[cfg(test)]
pub(crate) mod test_call_hook {
    use parking_lot::RwLock;
    use serde_json::Value;
    use std::sync::Arc;

    type Hook = Arc<
        dyn Fn(&str, &str, &serde_json::Map<String, Value>) -> anyhow::Result<Value>
            + Send
            + Sync,
    >;

    static HOOK: RwLock<Option<Hook>> = RwLock::new(None);

    pub(crate) struct Guard;

    pub(crate) fn install(hook: Hook) -> Guard {
        *HOOK.write() = Some(hook);
        Guard
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            *HOOK.write() = None;
        }
    }

    pub(crate) fn get() -> Option<Hook> {
        HOOK.read().clone()
    }
}

/// Resolve the named MCP server from the merged global+project store.
/// Returns the entry's transport config + a flag indicating which
/// store it came from (for diagnostics).
pub fn resolve_server(name: &str, project_dir: Option<&str>) -> Result<McpServerConfig> {
    let mut store = match global_store_path() {
        Some(p) => McpStore::load(&p).unwrap_or_else(|_| McpStore::new()),
        None => McpStore::new(),
    };
    if let Some(dir) = project_dir {
        let proj_path = project_store_path(std::path::Path::new(dir));
        if let Ok(proj) = McpStore::load(&proj_path) {
            for (k, v) in proj.servers {
                store.servers.insert(k, v);
            }
        }
    }
    store
        .servers
        .remove(name)
        .ok_or_else(|| anyhow!("MCP server '{name}' not in registry (global or project overlay)"))
}

/// Call a tool on the named server. Result is returned as a
/// serde_json::Value:
///   - `structured_content` if present (preferred — preserves typing)
///   - else the joined text content as a JSON string (parsed when it
///     parses; passed through verbatim otherwise)
///   - else Null
///
/// Tool error (`is_error: true`) is surfaced as Err with the
/// concatenated text content as the message.
///
/// `cwd` is honored only for stdio transports — passes through as the
/// child process's working directory. MCP servers like biofilter that
/// resolve project state via `process.cwd()` need this set; the
/// caller threads `vars.worktree_path` through the op's `args.cwd`.
pub async fn call_tool(
    server_name: &str,
    tool_name: &str,
    arguments: serde_json::Map<String, Value>,
    timeout_secs: u64,
    project_dir: Option<&str>,
    cwd: Option<&str>,
) -> Result<Value> {
    #[cfg(test)]
    if let Some(hook) = test_call_hook::get() {
        return hook(server_name, tool_name, &arguments);
    }
    let cfg = resolve_server(server_name, project_dir)?;
    let dur = Duration::from_secs(timeout_secs.max(1));
    let result = timeout(dur, do_call(cfg, tool_name, arguments, cwd)).await;
    match result {
        Ok(inner) => inner,
        Err(_) => bail!("mcp_call '{server_name}.{tool_name}' timed out after {timeout_secs}s"),
    }
}

async fn do_call(
    cfg: McpServerConfig,
    tool_name: &str,
    arguments: serde_json::Map<String, Value>,
    cwd: Option<&str>,
) -> Result<Value> {
    let resolved = cfg.resolve_secrets()?;
    match cfg {
        McpServerConfig::Stdio { command, args, .. } => {
            stdio_call(command, args, resolved.env, tool_name, arguments, cwd).await
        }
        McpServerConfig::Http { url, .. } | McpServerConfig::Sse { url, .. } => {
            http_call(url, resolved.headers, tool_name, arguments).await
        }
    }
}

async fn stdio_call(
    command: String,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    tool_name: &str,
    arguments: serde_json::Map<String, Value>,
    cwd: Option<&str>,
) -> Result<Value> {
    // Resolve $HOME / common shell variables in `command` so
    // `${CLAUDE_PLUGIN_ROOT}` and friends pulled from the operator's
    // mcp.json round-trip cleanly. We support a narrow subset on
    // purpose — full shell expansion would be a footgun.
    let resolved_cmd = expand_simple_env(&command);
    let resolved_args: Vec<String> = args.iter().map(|a| expand_simple_env(a)).collect();

    let mut command_builder = tokio::process::Command::new(&resolved_cmd);
    command_builder.args(&resolved_args);
    for (k, v) in &env {
        command_builder.env(k, expand_simple_env(v));
    }
    if let Some(dir) = cwd {
        command_builder.current_dir(expand_simple_env(dir));
    }
    let transport =
        TokioChildProcess::new(command_builder.configure(|_| {})).with_context(|| {
            format!(
                "spawning stdio MCP child '{} {}'",
                resolved_cmd,
                resolved_args.join(" ")
            )
        })?;
    let client = ()
        .serve(transport)
        .await
        .with_context(|| format!("MCP handshake failed for stdio server '{resolved_cmd}'"))?;
    // Always send an arguments object — even empty. Some servers (e.g.
    // biofilter's zod-validated schemas) require the field present;
    // omitting it produces `Input validation` (-32602) errors at the
    // server side. Empty-object is the right canonical form.
    let params = CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(arguments.into_iter().collect());
    let resp = client
        .call_tool(params)
        .await
        .with_context(|| format!("call_tool '{tool_name}'"))?;
    // The tool result is already in hand. Some transports, especially
    // loopback HTTP self-calls, can linger during shutdown; cleanup
    // must not consume the caller's whole mcp_call timeout.
    let mut client = client;
    let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    extract_result_value(resp)
}

async fn http_call(
    url: String,
    _headers: BTreeMap<String, String>,
    tool_name: &str,
    arguments: serde_json::Map<String, Value>,
) -> Result<Value> {
    // Headers from the registry config aren't yet wired through —
    // rmcp 1.4's StreamableHttpClientTransport::from_uri is the simple
    // path; the with-headers builder needs a reqwest::Client we'd have
    // to construct. For the SASTquatch self-call (loopback, no auth)
    // the simple path is fine. Add the reqwest builder when the first
    // remote-HTTP target needs auth headers.
    let transport = StreamableHttpClientTransport::from_uri(url.clone());
    let client = ()
        .serve(transport)
        .await
        .with_context(|| format!("MCP handshake failed for http server '{url}'"))?;
    // Always send an arguments object — even empty. Some servers (e.g.
    // biofilter's zod-validated schemas) require the field present;
    // omitting it produces `Input validation` (-32602) errors at the
    // server side. Empty-object is the right canonical form.
    let params = CallToolRequestParams::new(tool_name.to_string())
        .with_arguments(arguments.into_iter().collect());
    let resp = client
        .call_tool(params)
        .await
        .with_context(|| format!("call_tool '{tool_name}'"))?;
    // The tool result is already in hand. Some transports, especially
    // loopback HTTP self-calls, can linger during shutdown; cleanup
    // must not consume the caller's whole mcp_call timeout.
    let mut client = client;
    let _ = client.close_with_timeout(Duration::from_secs(2)).await;
    extract_result_value(resp)
}

/// Collapse a `CallToolResult` into a single JSON value the engine can
/// stuff into a var. Tool errors (`is_error: true`) become Err with
/// the joined text content as the message.
fn extract_result_value(r: CallToolResult) -> Result<Value> {
    let text_blob = collect_text(&r.content);
    if r.is_error.unwrap_or(false) {
        let msg = if text_blob.is_empty() {
            "tool returned is_error=true with no text content".to_string()
        } else {
            text_blob.clone()
        };
        bail!("MCP tool error: {msg}");
    }
    if let Some(sc) = r.structured_content {
        return Ok(sc);
    }
    if text_blob.is_empty() {
        return Ok(Value::Null);
    }
    // Try to round-trip the text as JSON; many MCP tools (incl. blackbox-
    // self and biofilter) emit JSON-stringified payloads in a single
    // text block. Fall back to the raw string if parse fails.
    match serde_json::from_str::<Value>(&text_blob) {
        Ok(v) => Ok(v),
        Err(_) => Ok(Value::String(text_blob)),
    }
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

/// Expand `${ENV_NAME}` and `$ENV_NAME` references against the
/// daemon's process env, plus the tilde shorthand for $HOME. Anything
/// else (escapes, defaults, command substitution, etc.) is left
/// verbatim — full shell expansion is intentionally out of scope.
fn expand_simple_env(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Tilde at start → $HOME
        if i == 0 && bytes[i] == b'~' && (bytes.len() == 1 || bytes[i + 1] == b'/') {
            if let Some(home) = dirs::home_dir() {
                out.push_str(&home.to_string_lossy());
            } else {
                out.push('~');
            }
            i += 1;
            continue;
        }
        // ${VAR}
        if i + 1 < bytes.len() && bytes[i] == b'$' && bytes[i + 1] == b'{' {
            if let Some(close) = bytes[i + 2..].iter().position(|&b| b == b'}') {
                let end = i + 2 + close;
                let name = &s[i + 2..end];
                match std::env::var(name) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => {
                        out.push_str("${");
                        out.push_str(name);
                        out.push('}');
                    }
                }
                i = end + 1;
                continue;
            }
        }
        // $VAR — POSIX-style, [A-Za-z_][A-Za-z0-9_]*
        if bytes[i] == b'$'
            && i + 1 < bytes.len()
            && (bytes[i + 1].is_ascii_alphabetic() || bytes[i + 1] == b'_')
        {
            let name_start = i + 1;
            let mut j = name_start;
            while j < bytes.len() && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_') {
                j += 1;
            }
            let name = &s[name_start..j];
            match std::env::var(name) {
                Ok(v) => out.push_str(&v),
                Err(_) => {
                    out.push('$');
                    out.push_str(name);
                }
            }
            i = j;
            continue;
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Test-only handle — keeps the resolved-path helpers reachable from
/// integration tests that exercise the registry overlay logic.
#[cfg(test)]
pub fn _resolve_for_test(name: &str, project_dir: Option<&str>) -> Result<McpServerConfig> {
    resolve_server(name, project_dir)
}

/// Test-only path: helper used by the unit tests for env expansion.
#[cfg(test)]
pub fn _expand_for_test(s: &str) -> String {
    expand_simple_env(s)
}

// `PathBuf` is used through `dirs::home_dir().to_string_lossy()` — the
// import keeps a single hit on the dependency graph clear.
#[allow(dead_code)]
fn _path_buf_anchor() -> Option<PathBuf> {
    dirs::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_expansion_handles_braced_form() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("MCP_TEST_VAR", "abc123");
        }
        assert_eq!(_expand_for_test("token ${MCP_TEST_VAR}"), "token abc123");
    }

    #[test]
    fn env_expansion_handles_unbraced_form() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::set_var("MCP_TEST_VAR2", "xyz");
        }
        assert_eq!(_expand_for_test("$MCP_TEST_VAR2/path"), "xyz/path");
    }

    #[test]
    fn env_expansion_leaves_missing_verbatim() {
        let _env = crate::util::test_env_lock();
        unsafe {
            std::env::remove_var("MCP_TEST_NOT_SET_42");
        }
        assert_eq!(
            _expand_for_test("${MCP_TEST_NOT_SET_42}"),
            "${MCP_TEST_NOT_SET_42}"
        );
    }

    #[test]
    fn env_expansion_resolves_tilde_at_start() {
        let home = dirs::home_dir().unwrap();
        let expanded = _expand_for_test("~/foo");
        assert!(expanded.starts_with(&*home.to_string_lossy()));
        assert!(expanded.ends_with("/foo"));
    }

    #[test]
    fn env_expansion_does_not_resolve_tilde_mid_path() {
        // `~` only resolves at index 0 — defends against accidental
        // expansion when a literal tilde appears in arg values.
        assert_eq!(_expand_for_test("foo~bar"), "foo~bar");
    }
}

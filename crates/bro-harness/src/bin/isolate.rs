//! `isolate` — standalone isolate-tool validator.
//!
//! Invoke any bro-harness isolate binding (`java.*`, `analysis.*`, `code.*`,
//! `edits.*`, `lsp.*`, plus the generic file/shell/glob builtins) directly
//! against a fixture/worktree to validate behavior — no agent loop, no LLM, no
//! daemon, no probe roundtrip. It builds a `ToolCx` rooted at `--root`,
//! resolves the tool by name, calls it, and prints the result.
//!
//! Pass `--mcp-config <PATH>` to expose MCP servers' tools as
//! `mcp__<server>__<tool>` alongside the builtins, so a cell can reach an
//! installed capability (e.g. `@playwright/mcp` → `tools.mcp__playwright__*`)
//! with no daemon — the same `load_mcp_tools` path the agent loop uses.
//!
//! Examples:
//!
//! ```text
//! isolate --list
//! isolate --root /tmp/fix --describe java.extractClassPreviewPlan
//! isolate --root /tmp/fix java.extractClassPreviewPlan \
//!   --args '{"file":"src/Foo.java","methods":["buildGrid"]}'
//! isolate --root /tmp/fix java.extractClassPreviewPlan --args-file args.json
//! isolate --root /tmp/fix --cell 'const r = await tools.file_read({file_path:"src/Foo.java"}); text(r);'
//! isolate --root /tmp/fix --cell-file setup.js --cell-file verify.js
//! isolate --root /tmp/fix --mcp-config .mcp.json --cell \
//!   'const r = await tools.mcp__playwright__browser_navigate({url:"https://example.com"}); text("ok");'
//! ```

use std::collections::BTreeMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::Parser;
use serde_json::{Value, json};

use bro_harness::bindings::BindingToolSession;
use bro_harness::bindings::namespace_descriptions;
use bro_harness::capabilities::HostTools;
use bro_harness::code_mode::{CodeMode, CodeModeToolSession};
use bro_harness::mcp::{ToolFilter, load_mcp_tools};
use bro_tools::builtin_tools;
use bro_tools::{
    EditSink, SafetyPolicy, ShellSessions, TodoList, Tool, ToolArgDefaults, ToolCx, ToolResult,
};

#[derive(Parser)]
#[command(
    name = "isolate",
    about = "Standalone isolate-tool validator — call a binding directly, no probe/LLM"
)]
struct Cli {
    /// List available tools (name) and exit.
    #[arg(long)]
    list: bool,

    /// Print a tool's input schema/contract and exit.
    #[arg(long, value_name = "TOOL")]
    describe: Option<String>,

    /// Worktree/fixture root the tool runs in (its cwd). Required to run a tool.
    #[arg(long, value_name = "DIR")]
    root: Option<PathBuf>,

    /// Tool to invoke, e.g. `java.extractClassPreviewPlan`.
    tool: Option<String>,

    /// JSON object of arguments for the tool.
    #[arg(long)]
    args: Option<String>,

    /// Read tool arguments from a JSON file.
    #[arg(long, value_name = "PATH")]
    args_file: Option<PathBuf>,

    /// Evaluate one JavaScript code-mode cell. May be repeated; repeated cells
    /// share one in-process session so store()/load() survives across cells.
    #[arg(long, value_name = "JS")]
    cell: Vec<String>,

    /// Read and evaluate one JavaScript code-mode cell from a file. May be
    /// repeated; repeated files share one in-process session.
    #[arg(long, value_name = "PATH")]
    cell_file: Vec<PathBuf>,

    /// Extract one field from a JSON result by JSON-pointer path
    /// (e.g. `/internal_helper_deps`). Errors if the path misses. Only applies
    /// to JSON results.
    #[arg(long, value_name = "POINTER")]
    field: Option<String>,

    /// Refuse to run if the tool's `file` argument is absent under --root —
    /// guards the silent empty-result footgun where a wrong path reads as an
    /// empty file instead of erroring.
    #[arg(long)]
    strict: bool,

    /// Load MCP servers from a `{"mcpServers":{...}}` JSON file and expose
    /// their tools as `mcp__<server>__<tool>`, so a cell can reach an installed
    /// capability (e.g. `@playwright/mcp` → `tools.mcp__playwright__*`) without
    /// the daemon. Best-effort: a server that can't be reached or listed is
    /// logged to stderr and skipped.
    #[arg(long, value_name = "PATH")]
    mcp_config: Option<PathBuf>,

    /// Max wall time per --cell/--cell-file, including automatic waits.
    #[arg(long, value_name = "SECONDS", default_value_t = 900)]
    cell_timeout: u64,

    /// Host tool-arg default table as a JSON string map
    /// ({"default:<tool>.<param>": "value", ...}), the same grammar as
    /// BRO_HARNESS_TOOL_DEFAULTS (which is the fallback when the flag is
    /// absent). Operator-authority grants (RX-V1) reach bindings through
    /// this channel, never as cell-authored arguments.
    #[arg(long, value_name = "JSON")]
    tool_defaults: Option<String>,
}

struct IsolateSurface {
    tools: Vec<Arc<dyn Tool>>,
    binding_session: BindingToolSession,
}

/// The full isolate surface: generic builtins (file/shell/glob/git) plus the
/// harness DSL bindings (code/edits/lsp/java/analysis), plus any MCP servers
/// named in `--mcp-config`. Mirrors the set a code-mode cell sees, minus the
/// agent-loop-only `exec`/`wait` wrappers. With `--mcp-config`, MCP tools
/// stand in for the daemon capability tools: a standalone cell reaches an
/// installed capability (`mcp__playwright__*`) directly, where the daemon path
/// injects them through its own MCP registry.
///
/// MCP loading is best-effort: a server that can't be reached or listed is
/// logged to stderr and skipped, so MCP unavailability never aborts a run.
async fn build_surface(mcp_config: Option<&str>) -> IsolateSurface {
    let binding_session = BindingToolSession::new();
    let mut tools = builtin_tools();
    tools.extend(binding_session.tools());
    if let Some(cfg) = mcp_config {
        // Permissive filter: a standalone validator admits everything a server
        // lists. The recursion guard is agent-loop-only — no nested dispatch
        // here to protect against.
        tools.extend(load_mcp_tools(Some(cfg), &ToolFilter::default()).await);
    }
    IsolateSurface {
        tools,
        binding_session,
    }
}

/// Minimal `ToolCx` rooted at `root` — same shape as the test helper, all
/// shared state defaulted. Read-only bindings only touch `root`; the shell/
/// edit sinks are wired so `edits.*`/`shell_run` also work when exercised.
fn make_cx(root: PathBuf, tool_arg_defaults: ToolArgDefaults) -> ToolCx {
    ToolCx {
        root,
        safety: Arc::new(SafetyPolicy::new()),
        http: reqwest::Client::new(),
        todos: Arc::new(StdMutex::new(TodoList::default())),
        shell_sessions: Arc::new(StdMutex::new(ShellSessions::default())),
        edits: Arc::new(StdMutex::new(EditSink::default())),
        session_env: Arc::new(BTreeMap::new()),
        shell_env: Arc::new(BTreeMap::new()),
        tool_arg_defaults: Arc::new(tool_arg_defaults),
    }
}

/// Load the host tool-arg default table: `--tool-defaults` JSON wins, then
/// the BRO_HARNESS_TOOL_DEFAULTS env var (same grammar as the harness's
/// --additional-context path), else empty.
fn load_tool_defaults(cli_json: Option<&str>) -> Result<ToolArgDefaults> {
    let raw = match cli_json {
        Some(raw) => serde_json::from_str::<BTreeMap<String, String>>(raw)
            .context("parse --tool-defaults as a JSON string map")?,
        None => match std::env::var("BRO_HARNESS_TOOL_DEFAULTS") {
            Ok(raw) if !raw.trim().is_empty() => serde_json::from_str(&raw)
                .context("parse BRO_HARNESS_TOOL_DEFAULTS as a JSON string map")?,
            _ => BTreeMap::new(),
        },
    };
    ToolArgDefaults::parse_map(raw).map_err(anyhow::Error::msg)
}

fn find_tool<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> Result<&'a Arc<dyn Tool>> {
    tools
        .iter()
        .find(|t| t.name() == name)
        .ok_or_else(|| anyhow!("unknown tool `{name}`; use --list to enumerate"))
}

/// Read a user-supplied CLI file at startup. Blocking fs is denied by the
/// workspace clippy gate except at sanctioned sites; this is the startup read
/// of a small, user-provided file.
#[allow(clippy::disallowed_methods)]
fn read_cli_file(path: &Path) -> Result<String> {
    Ok(std::fs::read_to_string(path)?)
}

fn reject_cell_direct_tool_mix(cli: &Cli) -> Result<()> {
    if cli.tool.is_some()
        || cli.args.is_some()
        || cli.args_file.is_some()
        || cli.field.is_some()
        || cli.strict
    {
        bail!(
            "--cell/--cell-file cannot be combined with direct tool invocation flags \
             (`<tool>`, --args, --args-file, --field, --strict)"
        );
    }
    if !cli.cell.is_empty() && !cli.cell_file.is_empty() {
        bail!("--cell and --cell-file cannot be mixed; repeat one of them to run multiple cells");
    }
    Ok(())
}

fn read_cell_sources(cli: &Cli) -> Result<Vec<String>> {
    reject_cell_direct_tool_mix(cli)?;
    if !cli.cell.is_empty() {
        return Ok(cli.cell.clone());
    }

    cli.cell_file
        .iter()
        .map(|path| {
            read_cli_file(path).with_context(|| format!("read --cell-file {}", path.display()))
        })
        .collect()
}

fn code_mode_session(cx: &ToolCx, callable: &[Arc<dyn Tool>]) -> CodeModeToolSession {
    let seam = Arc::new(HostTools::new(callable.to_vec(), cx.clone()));
    CodeModeToolSession::new(callable, seam, CodeMode::Only, &namespace_descriptions())
}

async fn execute_cell_sources(
    sources: Vec<String>,
    cx: &ToolCx,
    callable: &[Arc<dyn Tool>],
    cell_timeout: Duration,
) -> Result<Vec<ToolResult>> {
    let code_mode = code_mode_session(cx, callable);
    let code_mode_tools = code_mode.tools();
    let exec = find_tool(&code_mode_tools, bro_code_mode::PUBLIC_TOOL_NAME)?.clone();
    let wait = find_tool(&code_mode_tools, bro_code_mode::WAIT_TOOL_NAME)?.clone();
    let mut results = Vec::with_capacity(sources.len());

    let execution_result = async {
        for source in sources {
            let result = exec.call(json!({ "source": source }), cx).await;
            let result = drive_cell_to_completion(result, &wait, cx, cell_timeout).await?;
            let is_error = result.is_error();
            results.push(result);
            if is_error {
                break;
            }
        }
        Ok(results)
    }
    .await;

    let shutdown_result = code_mode.shutdown().await;
    match (execution_result, shutdown_result) {
        (Ok(results), Ok(())) => Ok(results),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(anyhow!("code-mode shutdown failed: {err}")),
    }
}

async fn drive_cell_to_completion(
    mut result: ToolResult,
    wait: &Arc<dyn Tool>,
    cx: &ToolCx,
    cell_timeout: Duration,
) -> Result<ToolResult> {
    let started = Instant::now();
    let mut accumulated = String::new();
    while let Some((cell_id, yielded_body)) = yielded_cell(&result) {
        append_cell_output(&mut accumulated, yielded_body);
        let elapsed = started.elapsed();
        if elapsed >= cell_timeout {
            let _ = wait
                .call(json!({ "cell_id": cell_id, "terminate": true }), cx)
                .await;
            bail!(
                "isolate: cell {cell_id} exceeded --cell-timeout {}s after {:.1}s",
                cell_timeout.as_secs(),
                elapsed.as_secs_f64()
            );
        }
        let remaining = cell_timeout.saturating_sub(elapsed);
        let wait_ms = remaining
            .as_millis()
            .min(u128::from(bro_code_mode::DEFAULT_WAIT_YIELD_TIME_MS))
            .max(1) as u64;
        result = wait
            .call(json!({ "cell_id": cell_id, "yield_time_ms": wait_ms }), cx)
            .await;
    }
    Ok(prepend_cell_output(result, accumulated))
}

fn yielded_cell(result: &ToolResult) -> Option<(String, &str)> {
    let ToolResult::Text(text) = result else {
        return None;
    };
    const MARKER: &str = "\n\nScript running with cell ID ";
    const SUFFIX: &str = ". Call `wait` with this cell_id for more output.";
    let marker_start = text.rfind(MARKER)?;
    let after_marker = &text[marker_start + MARKER.len()..];
    let suffix_start = after_marker.find(SUFFIX)?;
    if suffix_start + SUFFIX.len() != after_marker.len() {
        return None;
    }
    Some((
        after_marker[..suffix_start].to_string(),
        &text[..marker_start],
    ))
}

fn append_cell_output(accumulated: &mut String, chunk: &str) {
    if chunk.is_empty() {
        return;
    }
    if !accumulated.is_empty() {
        accumulated.push('\n');
    }
    accumulated.push_str(chunk);
}

fn prepend_cell_output(result: ToolResult, accumulated: String) -> ToolResult {
    if accumulated.is_empty() {
        return result;
    }
    match result {
        ToolResult::Text(text) if text.is_empty() => ToolResult::Text(accumulated),
        ToolResult::Text(text) => ToolResult::Text(format!("{accumulated}\n{text}")),
        ToolResult::Error(error) if error.is_empty() => ToolResult::Error(accumulated),
        ToolResult::Error(error) => ToolResult::Error(format!("{accumulated}\n{error}")),
        other => other,
    }
}

async fn shutdown_session_owned_children(cx: &ToolCx, surface: &IsolateSurface) {
    surface.binding_session.shutdown().await;
    if let Ok(mut sessions) = cx.shell_sessions.lock() {
        sessions.shutdown_all();
    }
}

fn emit_tool_result(result: ToolResult, field: Option<&str>) -> Result<bool> {
    match result {
        ToolResult::Json(v) => {
            if let Some(pointer) = field {
                match v.pointer(pointer) {
                    Some(found) => println!("{}", serde_json::to_string_pretty(found)?),
                    None => {
                        eprintln!("error: --field `{pointer}` not found in result");
                        return Ok(false);
                    }
                }
            } else {
                println!("{}", serde_json::to_string_pretty(&v)?);
            }
        }
        ToolResult::Text(t) => {
            if field.is_some() {
                eprintln!("error: --field requires a JSON result; tool returned text");
                return Ok(false);
            }
            println!("{t}");
        }
        ToolResult::Error(e) => {
            eprintln!("error: {e}");
            return Ok(false);
        }
    }
    Ok(true)
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();

    // gap-3cc07165: a relative --root silently breaks shell spawns inside
    // cells (the child cwd resolves against a different process cwd) with an
    // opaque os error 2. Canonicalize once so every consumer sees an
    // absolute, existing root.
    if let Some(root) = cli.root.take() {
        let canonical = root.canonicalize().map_err(|e| {
            anyhow!(
                "--root {}: cannot canonicalize ({e}); pass an existing directory",
                root.display()
            )
        })?;
        cli.root = Some(canonical);
    }

    let mcp_config = match &cli.mcp_config {
        Some(path) => Some(read_cli_file(path)?),
        None => None,
    };
    let surface = build_surface(mcp_config.as_deref()).await;
    let tool_defaults = load_tool_defaults(cli.tool_defaults.as_deref())?;

    if cli.list {
        let mut names: Vec<&str> = surface.tools.iter().map(|t| t.name()).collect();
        names.sort_unstable();
        for n in names {
            println!("{n}");
        }
        return Ok(());
    }

    if let Some(name) = cli.describe {
        let tool = find_tool(&surface.tools, &name)?;
        println!("{}", serde_json::to_string_pretty(&tool.input_schema())?);
        return Ok(());
    }

    let cell_mode = !cli.cell.is_empty() || !cli.cell_file.is_empty();
    if cell_mode {
        let root = cli
            .root
            .clone()
            .ok_or_else(|| anyhow!("--root <DIR> is required to evaluate a cell"))?;
        let sources = read_cell_sources(&cli)?;
        let cx = make_cx(root, tool_defaults);
        let execution = execute_cell_sources(
            sources,
            &cx,
            &surface.tools,
            Duration::from_secs(cli.cell_timeout),
        )
        .await;
        shutdown_session_owned_children(&cx, &surface).await;
        for result in execution? {
            if !emit_tool_result(result, None)? {
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let tool_name = cli.tool.ok_or_else(|| {
        anyhow!("no tool specified — use --list, --describe <TOOL>, or pass a tool name")
    })?;
    let root = cli
        .root
        .ok_or_else(|| anyhow!("--root <DIR> is required to run a tool"))?;
    let tool = find_tool(&surface.tools, &tool_name)?;

    let input: Value = if let Some(path) = cli.args_file {
        let raw =
            read_cli_file(&path).with_context(|| format!("read --args-file {}", path.display()))?;
        serde_json::from_str(&raw).context("--args-file is not valid JSON")?
    } else if let Some(raw) = cli.args {
        serde_json::from_str(&raw).context("--args is not valid JSON")?
    } else {
        json!({})
    };

    if cli.strict {
        if let Some(file) = input.get("file").and_then(Value::as_str) {
            let resolved = root.join(file);
            if !resolved.exists() {
                bail!(
                    "--strict: file `{file}` not found under root {}",
                    root.display()
                );
            }
        }
    }

    let cx = make_cx(root, tool_defaults);
    let result = tool.call(input, &cx).await;
    shutdown_session_owned_children(&cx, &surface).await;
    if !emit_tool_result(result, cli.field.as_deref())? {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cell_mode_preserves_kv_and_functions_across_cells() {
        let dir = tempfile::tempdir().unwrap();
        let cx = make_cx(dir.path().to_path_buf(), ToolArgDefaults::default());
        let surface = build_surface(None).await;
        let results = execute_cell_sources(
            vec![
                "store('k', { n: 7 }); store('helpers.double', (n) => n * 2);".to_string(),
                "const double = load('helpers.double'); text(`${load('k').n}:${double(21)}`);"
                    .to_string(),
            ],
            &cx,
            &surface.tools,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 2);
        match &results[1] {
            ToolResult::Text(t) => assert!(t.contains("7:42"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cell_mode_dispatches_nested_workspace_tools() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("probe.txt"), "hello from cell").unwrap();
        let cx = make_cx(dir.path().to_path_buf(), ToolArgDefaults::default());
        let surface = build_surface(None).await;
        let results = execute_cell_sources(
            vec![
                "const body = await tools.file_read({ file_path: 'probe.txt' }); text(body);"
                    .to_string(),
            ],
            &cx,
            &surface.tools,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        match &results[0] {
            ToolResult::Text(t) => assert!(t.contains("hello from cell"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cell_mode_auto_waits_until_a_yielded_cell_completes() {
        let dir = tempfile::tempdir().unwrap();
        let cx = make_cx(dir.path().to_path_buf(), ToolArgDefaults::default());
        let surface = build_surface(None).await;
        let results = execute_cell_sources(
            vec![
                r#"// @exec: {"yield_time_ms": 1}
text("before");
await new Promise((resolve) => setTimeout(resolve, 20));
text("after");"#
                    .to_string(),
            ],
            &cx,
            &surface.tools,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        match &results[0] {
            ToolResult::Text(t) => assert_eq!(t, "before\nafter"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    /// `build_surface` wiring proof: an MCP server's tools merge into the
    /// callable set and a cell dispatches one through the HostTools seam — the
    /// same path a real `@playwright/mcp` tool takes. Uses an in-process
    /// surface so it needs no spawned server.
    #[tokio::test]
    async fn cell_dispatches_an_mcp_tool_merged_into_the_surface() {
        use async_trait::async_trait;
        use bro_harness::mcp::{
            McpConfig, McpServerConfig, McpSurface, McpToolSpec, ToolPlacementMap,
            load_mcp_tools_from_config,
        };

        struct EchoSurface;
        #[async_trait]
        impl McpSurface for EchoSurface {
            async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
                Ok(vec![McpToolSpec {
                    name: "echo".to_string(),
                    description: "echo the input".to_string(),
                    input_schema: json!({"type": "object"}),
                }])
            }
            async fn call_tool(&self, tool: &str, input: Value) -> anyhow::Result<ToolResult> {
                Ok(ToolResult::Json(json!({ "tool": tool, "got": input })))
            }
        }

        let cfg = McpConfig {
            servers: vec![McpServerConfig::InProcess {
                name: "fake".to_string(),
                server: Arc::new(EchoSurface),
            }],
            tool_placement: ToolPlacementMap::new(),
        };
        let mut tools = builtin_tools();
        tools.extend(BindingToolSession::new().tools());
        tools.extend(load_mcp_tools_from_config(&cfg, &ToolFilter::default()).await);
        assert!(
            tools.iter().any(|t| t.name() == "mcp__fake__echo"),
            "MCP tool must merge into the surface"
        );

        let dir = tempfile::tempdir().unwrap();
        let cx = make_cx(dir.path().to_path_buf(), ToolArgDefaults::default());
        let results = execute_cell_sources(
            vec![
                "const r = await tools.mcp__fake__echo({ a: 1 }); text(JSON.stringify(r));"
                    .to_string(),
            ],
            &cx,
            &tools,
            Duration::from_secs(5),
        )
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        match &results[0] {
            ToolResult::Text(t) => assert!(t.contains("\"a\":1"), "got: {t}"),
            other => panic!("expected text, got {other:?}"),
        }
    }
}

#![allow(
    clippy::collapsible_if,
    clippy::doc_overindented_list_items,
    clippy::doc_lazy_continuation,
    clippy::too_many_arguments,
    clippy::type_complexity,
    clippy::large_enum_variant,
    clippy::enum_variant_names,
    clippy::let_and_return
)]

//! `bro tail` — headless stream printer for agent orchestration.
//!
//! Selects one or more bros (by name, team, session, or provider), subscribes
//! to the daemon's `/tail` SSE stream, and writes each event payload to stdout
//! for piping, scripting, and logging. The interactive cockpit lives in
//! `bro fleet`.

use std::io::{self, Write};
use std::time::Duration;

use bro_fleet_client::Provider;

use clap::{Args, Parser, Subcommand};
use ratatui::prelude::{Color, Line, Modifier, Span, Style};

mod fleet_classifier;
mod fleet_tui;
mod logging;
mod mcp_call;
#[cfg(test)]
mod test_backend;

#[derive(Default, Debug, Clone)]
struct TailSelectors {
    bros: Vec<String>,
    teams: Vec<String>,
    sessions: Vec<String>,
    providers: Vec<String>,
}

#[derive(Debug, Parser)]
#[command(
    name = "bro",
    about = "Terminal client for blackbox orchestration",
    version
)]
struct BroCli {
    #[command(subcommand)]
    command: BroCommand,
}

#[derive(Debug, Subcommand)]
enum BroCommand {
    /// Print daemon tail SSE payloads to stdout
    Tail(TailArgs),
    /// Workflow orchestration — drive a mermaid-shaped flow through the daemon
    Orchestrate(OrchestrateArgs),
    /// MCP helpers - call daemon tools over streamable HTTP
    Mcp(mcp_call::McpArgs),
    /// Fleet cockpit — dispatch and live-drive many top-level agents
    Fleet(FleetArgs),
    /// Single-agent cockpit — launch one agent into the Fleet transcript view
    Agent(AgentArgs),
}

#[derive(Debug, Args)]
struct FleetArgs {
    /// Default working directory for dispatched agents. Defaults to the
    /// cockpit's launch cwd.
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
    /// Daemon base URL for daemon-backed fleet dispatch/control. Also accepted
    /// via BLACKBOX_FLEET_DAEMON_URL.
    #[arg(long, value_name = "URL")]
    daemon_url: Option<String>,
    /// Bypass the single-cockpit instance lock and start even if another
    /// `bro fleet` already drives this fleet store. The two cockpits will
    /// fight over the roster stream — recovery use only.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Args)]
struct AgentArgs {
    /// Working directory for the agent. Defaults to the shell's launch cwd.
    #[arg(long, value_name = "DIR")]
    cwd: Option<String>,
    /// Provider to launch. Defaults to brodex, matching bro fleet.
    #[arg(long, default_value = "brodex")]
    provider: Provider,
    /// Provider model id/alias. Defaults to the provider catalog default.
    #[arg(long)]
    model: Option<String>,
    /// Provider effort/thinking level. Defaults to the provider catalog default.
    #[arg(long)]
    effort: Option<String>,
    /// Resume an existing provider session id instead of starting a fresh one.
    #[arg(long, value_name = "SESSION_ID")]
    resume: Option<String>,
    /// Optional first prompt / resume turn. If omitted, the TUI opens empty and
    /// dispatches or resumes when you submit the first composer line.
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    prompt: Vec<String>,
}

#[derive(Debug, Args)]
#[command(
    after_help = "Subcommands:\n  run <workflow.json>    dispatch a workflow and print event log"
)]
struct OrchestrateArgs {
    #[command(subcommand)]
    command: OrchestrateCommand,
}

#[derive(Debug, Subcommand)]
enum OrchestrateCommand {
    /// Load a workflow spec, POST it to the daemon, print the event log
    Run(OrchestrateRunArgs),
    /// Read an arc thread's note trail + latest compaction anchor
    Status(OrchestrateStatusArgs),
    /// List recent workflow arcs with their final status + latest anchor
    List(OrchestrateListArgs),
    /// Peek at in-flight arcs' live state (current node, visit counts, in_flight)
    Peek(OrchestratePeekArgs),
}

#[derive(Debug, Args)]
struct OrchestratePeekArgs {
    /// Arc thread ID to peek at. Omit to list all live arc snapshots.
    #[arg(value_name = "THREAD_ID")]
    thread_id: Option<String>,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct OrchestrateListArgs {
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
    /// Max entries to print (default: 20, most recent first).
    #[arg(long, default_value = "20")]
    limit: usize,
}

#[derive(Debug, Args)]
struct OrchestrateStatusArgs {
    /// Arc thread ID (e.g. `thread-9e03d596`). Returned by the earlier
    /// `run` invocation as `arc_thread_id`.
    #[arg(value_name = "THREAD_ID")]
    thread_id: String,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
}

#[derive(Debug, Args)]
struct OrchestrateRunArgs {
    /// Path to the workflow JSON file.
    #[arg(value_name = "WORKFLOW_JSON")]
    path: std::path::PathBuf,
    /// Daemon URL. Defaults to http://127.0.0.1:${BBOX_PORT:-7264}.
    #[arg(long)]
    url: Option<String>,
    /// Working directory passed to every dispatched bro.
    #[arg(long)]
    project_dir: Option<String>,
    /// Cap on activity-node steps. Defaults to 50 server-side.
    #[arg(long)]
    max_steps: Option<usize>,
    /// Validate + summarize the workflow without dispatching any bros.
    /// Prints the plan and exits.
    #[arg(long)]
    dry_run: bool,
    /// Stream events as they happen via SSE instead of blocking until
    /// completion. Useful for long arcs where you want live progress.
    #[arg(long)]
    stream: bool,
}

#[derive(Debug, Clone, Args)]
#[command(
    after_help = "Prints one SSE data payload per stdout line. Selectors are unioned and each flag is repeatable.\n\nExamples:\n  bro tail alice bob\n  bro tail --team review-panel\n  bro tail --team A --team B\n  bro tail --team A --bro solo --bro qa\n  bro tail --session <uuid>\n  bro tail --provider codex"
)]
struct TailArgs {
    /// Specific bros to include. Accepts bare names or `team::bro`.
    #[arg(long = "bro", value_name = "NAME")]
    bros: Vec<String>,
    /// Teams to include in full.
    #[arg(long = "team", value_name = "NAME")]
    teams: Vec<String>,
    /// Raw sessions to tail directly.
    #[arg(long = "session", value_name = "ID")]
    sessions: Vec<String>,
    /// Provider filter applied after selector union.
    #[arg(long = "provider", value_name = "NAME")]
    providers: Vec<String>,
    /// Positional shorthand for bro selectors.
    #[arg(value_name = "BRO")]
    positional_bros: Vec<String>,
}

impl From<TailArgs> for TailSelectors {
    fn from(args: TailArgs) -> Self {
        let mut bros = args.bros;
        bros.extend(args.positional_bros);
        Self {
            bros,
            teams: args.teams,
            sessions: args.sessions,
            providers: args.providers,
        }
    }
}

// ── Tail stream printer ─────────────────────────────────────────────

fn tail_url(sel: &TailSelectors) -> String {
    let port = bro_fleet_client::daemon_port();
    let mut url = format!("http://127.0.0.1:{port}/tail");
    let mut params = Vec::new();
    if !sel.bros.is_empty() {
        params.push(format!("bros={}", sel.bros.join(",")));
    }
    if !sel.teams.is_empty() {
        params.push(format!("teams={}", sel.teams.join(",")));
    }
    if !sel.sessions.is_empty() {
        params.push(format!("sessions={}", sel.sessions.join(",")));
    }
    if !sel.providers.is_empty() {
        params.push(format!("providers={}", sel.providers.join(",")));
    }
    if !params.is_empty() {
        url.push('?');
        url.push_str(&params.join("&"));
    }
    url
}

async fn run_tail_stream_printer(sel: TailSelectors) -> anyhow::Result<()> {
    use futures_util::StreamExt;

    let url = tail_url(&sel);
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .build()?;

    loop {
        let resp = match client
            .get(&url)
            .header("Accept", "text/event-stream")
            .send()
            .await
        {
            Ok(resp) if resp.status().is_success() => resp,
            Ok(resp) => {
                eprintln!("/tail returned {}; retrying", resp.status());
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            Err(err) => {
                eprintln!("cannot reach blackboxd tail stream: {err}; retrying");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut stdout = io::stdout().lock();
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    eprintln!("tail stream ended with error: {err}; reconnecting");
                    break;
                }
            };
            buf.extend_from_slice(&chunk);
            while let Some(pos) = find_sse_separator(&buf) {
                let frame_bytes = buf.drain(..pos + 2).collect::<Vec<_>>();
                let frame = String::from_utf8_lossy(&frame_bytes);
                if !write_tail_frame(&mut stdout, &frame)? {
                    return Ok(());
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

fn write_tail_frame(out: &mut impl Write, frame: &str) -> io::Result<bool> {
    let mut payload = String::new();
    for line in frame.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(rest) = line.strip_prefix("data:") {
            payload.push_str(rest.trim_start());
        }
    }
    if payload.is_empty() {
        return Ok(true);
    }
    if writeln!(out, "{payload}").is_err() {
        return Ok(false);
    }
    if out.flush().is_err() {
        return Ok(false);
    }
    Ok(true)
}

async fn run_orchestrate(args: OrchestrateArgs) -> anyhow::Result<()> {
    match args.command {
        OrchestrateCommand::Run(run_args) => orchestrate_run(run_args).await,
        OrchestrateCommand::Status(status_args) => orchestrate_status(status_args).await,
        OrchestrateCommand::List(list_args) => orchestrate_list(list_args).await,
        OrchestrateCommand::Peek(peek_args) => orchestrate_peek(peek_args).await,
    }
}

fn default_base_url() -> anyhow::Result<String> {
    Ok(format!(
        "http://127.0.0.1:{}",
        bro_fleet_client::daemon_port()
    ))
}

async fn orchestrate_peek(args: OrchestratePeekArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let mut url = format!("{}/orchestrate/peek", base_url.trim_end_matches('/'));
    if let Some(tid) = &args.thread_id {
        url.push_str(&format!("?thread_id={}", urlencoding_lite(tid)));
    }
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let text = resp.text().await?;
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(err) = parsed["error"].as_str() {
        eprintln!("{err}");
        std::process::exit(1);
    }
    let snapshots: Vec<serde_json::Value> = if parsed.is_array() {
        parsed.as_array().cloned().unwrap_or_default()
    } else {
        vec![parsed]
    };
    if snapshots.is_empty() {
        println!("no live arc snapshots");
        return Ok(());
    }
    for s in snapshots {
        println!(
            "arc: {} ({}) v{}",
            s["arc_thread_id"].as_str().unwrap_or("?"),
            s["workflow_name"].as_str().unwrap_or("?"),
            s["workflow_version"].as_u64().unwrap_or(0)
        );
        println!("  status:    {}", s["status"].as_str().unwrap_or("?"));
        println!(
            "  current:   {}",
            s["current_node"].as_str().unwrap_or("(none)")
        );
        println!(
            "  completed: {}",
            s["completed_nodes"]
                .as_array()
                .map(|a| a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default()
        );
        let in_flight = s["in_flight_nodes"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        if !in_flight.is_empty() {
            println!("  in_flight: {in_flight}");
        }
        if let Some(v) = s["last_verdict"].as_str() {
            println!("  verdict:   {v}");
        }
        if let Some(vc) = s["visit_counts"].as_object() {
            let mut pairs: Vec<String> = vc.iter().map(|(k, v)| format!("{k}={v}")).collect();
            pairs.sort();
            println!("  visits:    {}", pairs.join(", "));
        }
        println!("  started:   {}", s["started_at"].as_str().unwrap_or("?"));
        println!("  updated:   {}", s["updated_at"].as_str().unwrap_or("?"));
        println!();
    }
    Ok(())
}

async fn orchestrate_list(args: OrchestrateListArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let url = format!("{}/orchestrate/list", base_url.trim_end_matches('/'));
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    let entries: serde_json::Value = serde_json::from_str(&text)?;
    let Some(arr) = entries.as_array() else {
        println!("no arcs");
        return Ok(());
    };
    if arr.is_empty() {
        println!("no workflow arcs found");
        return Ok(());
    }
    println!(
        "{:<18} {:<22} {:<10} {:<22} anchor",
        "thread_id", "name", "status", "last_activity"
    );
    for e in arr.iter().take(args.limit) {
        let tid = e["thread_id"].as_str().unwrap_or("?");
        let name = e["name"].as_str().unwrap_or("?");
        let status = e["final_status"]
            .as_str()
            .unwrap_or_else(|| e["status"].as_str().unwrap_or("?"));
        let last = e["last_activity"].as_str().unwrap_or("");
        let anchor_full = e["latest_anchor"].as_str().unwrap_or("");
        let anchor: String = anchor_full.chars().take(60).collect();
        println!("{tid:<18} {name:<22} {status:<10} {last:<22} {anchor}");
    }
    if arr.len() > args.limit {
        println!(
            "\n... {} more (use --limit to see more)",
            arr.len() - args.limit
        );
    }
    Ok(())
}

async fn orchestrate_status(args: OrchestrateStatusArgs) -> anyhow::Result<()> {
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let url = format!(
        "{}/orchestrate/status?thread_id={}",
        base_url.trim_end_matches('/'),
        urlencoding_lite(&args.thread_id)
    );
    let client = reqwest::Client::new();
    let resp = client.get(&url).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}");
        eprintln!("{text}");
        std::process::exit(1);
    }
    let parsed: serde_json::Value = serde_json::from_str(&text)?;
    println!(
        "arc thread: {}",
        parsed["thread_id"].as_str().unwrap_or("?")
    );
    if let Some(anchor) = parsed["latest_anchor"].as_str() {
        println!();
        println!("latest anchor:");
        println!("  {anchor}");
    }
    println!();
    println!("notes:");
    if let Some(notes) = parsed["notes"].as_array() {
        for n in notes {
            let kind = n["kind"].as_str().unwrap_or("?");
            let body = n["body"].as_str().unwrap_or("");
            let ts = n["created_at"].as_str().unwrap_or("");
            let id = n["id"].as_str().unwrap_or("?");
            let resolution = n["resolution"].as_str().unwrap_or("?");
            println!("  [{ts}] {id} {kind}/{resolution}");
            for line in body.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

/// Minimal RFC 3986 component-encoder for the one query param we send
/// (`thread_id` — already in canonical `thread-<8hex>` form but defensive
/// encoding costs nothing).
fn urlencoding_lite(s: &str) -> String {
    s.chars()
        .flat_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' {
                vec![c]
            } else {
                format!("%{:02X}", c as u32).chars().collect::<Vec<_>>()
            }
        })
        .collect()
}

async fn orchestrate_run(args: OrchestrateRunArgs) -> anyhow::Result<()> {
    use anyhow::Context;
    let workflow_raw = std::fs::read_to_string(&args.path)
        .with_context(|| format!("reading {}", args.path.display()))?;
    let workflow: serde_json::Value = serde_json::from_str(&workflow_raw)
        .with_context(|| format!("parsing {} as JSON", args.path.display()))?;
    let base_url = args
        .url
        .unwrap_or_else(|| default_base_url().unwrap_or_else(|_| "http://127.0.0.1:7264".into()));
    let mut body = serde_json::json!({ "workflow": workflow });
    if let Some(pd) = args.project_dir {
        body["project_dir"] = serde_json::Value::String(pd);
    }
    if let Some(ms) = args.max_steps {
        body["max_steps"] = serde_json::Value::from(ms);
    }
    if args.dry_run {
        body["dry_run"] = serde_json::Value::Bool(true);
    }

    if args.stream && !args.dry_run {
        return orchestrate_run_stream(&base_url, &body).await;
    }

    let url = format!("{}/orchestrate", base_url.trim_end_matches('/'));
    eprintln!("POST {url}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()?;
    let resp = client.post(&url).json(&body).send().await?;
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        eprintln!("daemon returned {status}");
        eprintln!("{text}");
        std::process::exit(1);
    }
    let parsed: serde_json::Value =
        serde_json::from_str(&text).with_context(|| "daemon returned non-JSON response")?;
    println!("status: {}", parsed["status"].as_str().unwrap_or("?"));
    println!();
    if let Some(plan) = parsed["plan"].as_str() {
        println!("{plan}");
        return Ok(());
    }
    if let Some(events) = parsed["events"].as_array() {
        for ev in events {
            let kind = ev["kind"].as_str().unwrap_or("?");
            let ts = ev["timestamp"].as_str().unwrap_or("");
            let data = ev["data"].to_string();
            println!("[{ts}] {kind}: {data}");
        }
    }
    println!();
    if let Some(outputs) = parsed["node_outputs"].as_object() {
        for (node, output) in outputs {
            let preview: String = output.as_str().unwrap_or("").chars().take(500).collect();
            println!("─── {node} ───");
            println!("{preview}");
            println!();
        }
    }
    Ok(())
}

async fn orchestrate_run_stream(base_url: &str, body: &serde_json::Value) -> anyhow::Result<()> {
    use futures_util::StreamExt;
    let url = format!("{}/orchestrate/stream", base_url.trim_end_matches('/'));
    eprintln!("POST {url} (streaming)");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3600))
        .build()?;
    let resp = client.post(&url).json(body).send().await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        eprintln!("daemon returned {status}\n{text}");
        std::process::exit(1);
    }
    // Parse SSE: each frame is `data: <json>\n\n`. Buffer chunks and
    // split on the SSE record separator.
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(pos) = find_sse_separator(&buf) {
            let frame_bytes = buf.drain(..pos + 2).collect::<Vec<_>>();
            let frame_str = String::from_utf8_lossy(&frame_bytes);
            for line in frame_str.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    print_stream_event(data);
                } else if let Some(data) = line.strip_prefix("data:") {
                    print_stream_event(data.trim_start());
                }
            }
        }
    }
    // Any trailing buffer (no terminator) — ignore; complete frames only.
    Ok(())
}

fn find_sse_separator(buf: &[u8]) -> Option<usize> {
    // SSE record terminator is `\n\n`. Some servers use `\r\n\r\n`;
    // handle both.
    for i in 0..buf.len().saturating_sub(1) {
        if buf[i] == b'\n' && buf[i + 1] == b'\n' {
            return Some(i);
        }
    }
    for i in 0..buf.len().saturating_sub(3) {
        if &buf[i..i + 4] == b"\r\n\r\n" {
            return Some(i + 2);
        }
    }
    None
}

fn print_stream_event(data: &str) {
    // Attempt structured parse for pretty output; fall back to raw.
    match serde_json::from_str::<serde_json::Value>(data) {
        Ok(ev) => {
            let kind = ev["kind"].as_str().unwrap_or("?");
            let ts = ev["timestamp"].as_str().unwrap_or("");
            if kind == "result" {
                println!("\n=== terminal ===");
                let result = &ev["data"];
                let status = result["status"].as_str().unwrap_or("?");
                println!("status: {status}");
                if let Some(arc) = result["arc_thread_id"].as_str() {
                    println!("arc_thread_id: {arc}");
                }
                if let Some(outputs) = result["node_outputs"].as_object() {
                    for (node, out) in outputs {
                        let preview: String =
                            out.as_str().unwrap_or("").chars().take(500).collect();
                        println!("\n─── {node} ───\n{preview}");
                    }
                }
            } else {
                let data_s = ev["data"].to_string();
                println!("[{ts}] {kind}: {data_s}");
            }
        }
        Err(_) => println!("{data}"),
    }
}

/// Capture each fleet harness session's stdout transcript + stderr (including
/// the turn-end diagnostic) to `<state>/bro/fleet/logs/<task>.{stdout.jsonl,
/// stderr.log}` so spurious-stop turns can be diagnosed postmortem. Honors an
/// operator-set `BLACKBOX_HARNESS_TEE_DIR` (export it empty to disable).
fn default_fleet_harness_tee() {
    if std::env::var_os("BLACKBOX_HARNESS_TEE_DIR").is_some() {
        return;
    }
    let base = std::env::var_os("BLACKBOX_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_default()
                .join(".local/state/blackbox")
        });
    let dir = base.join("bro").join("fleet").join("logs");
    // SAFETY: set once at CLI startup before the fleet dispatches any harness;
    // no other thread reads the environment at this point.
    unsafe { std::env::set_var("BLACKBOX_HARNESS_TEE_DIR", &dir) };
}

fn main() -> anyhow::Result<()> {
    let cli = BroCli::parse();
    let rt = tokio::runtime::Runtime::new()?;

    let result = match cli.command {
        BroCommand::Tail(args) => rt.block_on(run_tail_stream_printer(TailSelectors::from(args))),
        BroCommand::Orchestrate(args) => rt.block_on(run_orchestrate(args)),
        BroCommand::Mcp(args) => rt.block_on(mcp_call::run(args)),
        BroCommand::Fleet(args) => {
            default_fleet_harness_tee();
            rt.block_on(fleet_tui::run(args.cwd, args.daemon_url, args.force))
        }
        BroCommand::Agent(args) => {
            default_fleet_harness_tee();
            let prompt = (!args.prompt.is_empty()).then(|| args.prompt.join(" "));
            let launch = fleet_tui::AgentLaunch {
                cwd: args.cwd,
                provider: args.provider,
                model: args.model,
                effort: args.effort,
                resume: args.resume,
                prompt,
            };
            rt.block_on(fleet_tui::run_agent(launch))
        }
    };
    drop(rt);
    result
}

/// Convert a `ratatui_core` Line from tui-markdown output into a `'static`
/// ratatui Line that the fleet widgets consume.
pub(crate) fn line_into_owned<'a>(line: ratatui_core::text::Line<'a>) -> Line<'static> {
    let mut line_style = convert_core_style(line.style);
    let mut iter = line.spans.into_iter().peekable();
    let is_heading = iter
        .peek()
        .is_some_and(|s| is_heading_marker_span(&s.content));
    if is_heading {
        let _ = iter.next();
        line_style = line_style.add_modifier(Modifier::UNDERLINED);
    }
    let spans: Vec<Span<'static>> = iter
        .map(|s| {
            let merged = line_style.patch(convert_core_style(s.style));
            Span::styled(s.content.into_owned(), merged)
        })
        .collect();
    Line::from(spans)
}

fn is_heading_marker_span(s: &str) -> bool {
    let t = s.trim_end();
    !t.is_empty() && t.chars().all(|c| c == '#')
}

fn is_ordered_list_marker_only(line: &Line<'_>) -> bool {
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    let t = joined.trim();
    if t.is_empty() {
        return false;
    }
    let Some(dot_pos) = t.find('.') else {
        return false;
    };
    if dot_pos == 0 || dot_pos != t.len() - 1 {
        return false;
    }
    t[..dot_pos].chars().all(|c| c.is_ascii_digit())
}

fn is_bullet_marker_only(line: &Line<'_>) -> bool {
    let joined: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
    matches!(joined.trim(), "-" | "*" | "+")
}

pub(crate) fn stitch_list_markers(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut iter = lines.into_iter().peekable();
    while let Some(line) = iter.next() {
        if (is_ordered_list_marker_only(&line) || is_bullet_marker_only(&line))
            && let Some(next) = iter.next()
        {
            let mut spans = line.spans;
            spans.extend(next.spans);
            out.push(Line::from(spans));
            continue;
        }
        out.push(line);
    }
    out
}

fn convert_core_style(s: ratatui_core::style::Style) -> Style {
    let mut out = Style::default();
    if let Some(fg) = s.fg {
        out = out.fg(convert_core_color(fg));
    }
    if let Some(bg) = s.bg {
        out = out.bg(convert_core_color(bg));
    }
    out = out.add_modifier(Modifier::from_bits_truncate(s.add_modifier.bits()));
    out = out.remove_modifier(Modifier::from_bits_truncate(s.sub_modifier.bits()));
    out
}

fn convert_core_color(c: ratatui_core::style::Color) -> Color {
    use ratatui_core::style::Color as Rc;
    match c {
        Rc::Reset => Color::Reset,
        Rc::Black => Color::Black,
        Rc::Red => Color::Red,
        Rc::Green => Color::Green,
        Rc::Yellow => Color::Yellow,
        Rc::Blue => Color::Blue,
        Rc::Magenta => Color::Magenta,
        Rc::Cyan => Color::Cyan,
        Rc::Gray => Color::Gray,
        Rc::DarkGray => Color::DarkGray,
        Rc::LightRed => Color::LightRed,
        Rc::LightGreen => Color::LightGreen,
        Rc::LightYellow => Color::LightYellow,
        Rc::LightBlue => Color::LightBlue,
        Rc::LightMagenta => Color::LightMagenta,
        Rc::LightCyan => Color::LightCyan,
        Rc::White => Color::White,
        Rc::Rgb(r, g, b) => Color::Rgb(r, g, b),
        Rc::Indexed(i) => Color::Indexed(i),
    }
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn clap_parses_tail_repeatable_and_positional_selectors() {
        let cli = BroCli::parse_from([
            "bro",
            "tail",
            "alpha",
            "beta",
            "--team",
            "red",
            "--team",
            "blue",
            "--bro",
            "solo",
            "--session",
            "sid-123",
            "--provider",
            "gemini",
        ]);

        let BroCommand::Tail(args) = cli.command else {
            panic!("expected Tail command");
        };
        let sel = TailSelectors::from(args);
        assert_eq!(sel.bros, vec!["solo", "alpha", "beta"]);
        assert_eq!(sel.teams, vec!["red", "blue"]);
        assert_eq!(sel.sessions, vec!["sid-123"]);
        assert_eq!(sel.providers, vec!["gemini"]);
    }

    #[test]
    fn clap_preserves_scoped_bro_selectors() {
        let cli = BroCli::parse_from(["bro", "tail", "--bro", "red::reviewer"]);
        let BroCommand::Tail(args) = cli.command else {
            panic!("expected Tail command");
        };
        let sel = TailSelectors::from(args);
        assert_eq!(sel.bros, vec!["red::reviewer"]);
    }

    #[test]
    fn clap_parses_agent_launch_args() {
        // `glm` (a surviving provider): `claude` and the other CLI-shaped
        // providers were dropped in §4, so clap's value parser rejects them.
        let cli = BroCli::parse_from([
            "bro",
            "agent",
            "--provider",
            "glm",
            "--model",
            "sonnet",
            "--effort",
            "high",
            "--cwd",
            "/tmp/project",
            "write",
            "tests",
        ]);
        let BroCommand::Agent(args) = cli.command else {
            panic!("expected Agent command");
        };
        assert_eq!(args.provider, Provider::Glm);
        assert_eq!(args.model.as_deref(), Some("sonnet"));
        assert_eq!(args.effort.as_deref(), Some("high"));
        assert_eq!(args.cwd.as_deref(), Some("/tmp/project"));
        assert_eq!(args.prompt, vec!["write", "tests"]);
    }

    #[test]
    fn clap_agent_defaults_to_brodex() {
        let cli = BroCli::parse_from(["bro", "agent"]);
        let BroCommand::Agent(args) = cli.command else {
            panic!("expected Agent command");
        };
        assert_eq!(args.provider, Provider::Brodex);
        assert!(args.prompt.is_empty());
    }
}

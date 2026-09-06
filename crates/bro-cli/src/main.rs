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

mod blame;
mod fleet_classifier;
mod fleet_tui;
mod logging;
mod mcp_call;
mod provenance;
mod render_global;
#[cfg(test)]
mod test_backend;
mod workspace_binding;

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
    /// MCP helpers - call daemon tools over streamable HTTP
    Mcp(mcp_call::McpArgs),
    /// Guidance renders - pull the daemon's global render onto this host
    Render(render_global::RenderArgs),
    /// Checkout-local provenance commands
    Provenance(provenance::ProvenanceArgs),
    /// Checkout-local Git blame with central corpus enrichment
    Blame(blame::BlameArgs),
    /// Operator workspace binding lifecycle for one local checkout
    #[command(name = "workspace-binding")]
    WorkspaceBinding(workspace_binding::WorkspaceBindingArgs),
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
    /// Resume an existing provider session id or `/rename`d session name.
    #[arg(long, value_name = "SESSION_ID_OR_NAME")]
    resume: Option<String>,
    /// Optional first prompt / resume turn. If omitted, the TUI opens empty and
    /// dispatches or resumes when you submit the first composer line.
    #[arg(value_name = "PROMPT", trailing_var_arg = true)]
    prompt: Vec<String>,
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
    restore_default_sigpipe();
    let cli = BroCli::parse();
    let rt = tokio::runtime::Runtime::new()?;

    let result = match cli.command {
        BroCommand::Tail(args) => rt.block_on(run_tail_stream_printer(TailSelectors::from(args))),
        BroCommand::Mcp(args) => rt.block_on(mcp_call::run(args)),
        BroCommand::Render(args) => rt.block_on(render_global::run(args)),
        BroCommand::Provenance(args) => rt.block_on(provenance::run(args)),
        BroCommand::Blame(args) => rt.block_on(blame::run(args)),
        BroCommand::WorkspaceBinding(args) => rt.block_on(workspace_binding::run(args)),
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

/// Rust ignores SIGPIPE at startup so writes to a closed pipe surface as
/// `EPIPE` errors, which `println!` turns into a panic ("failed printing to
/// stdout: Broken pipe"). For a CLI that is routinely piped into `jq`, `head`,
/// or a script that exits early, the Unix-conventional behavior is to die
/// quietly with SIGPIPE, so restore the default disposition before printing
/// anything.
#[cfg(unix)]
fn restore_default_sigpipe() {
    // SAFETY: called once at process start before any threads exist; setting a
    // signal disposition has no memory-safety preconditions.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
fn restore_default_sigpipe() {}

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

    #[test]
    fn clap_routes_provenance_export() {
        let cli = BroCli::parse_from([
            "bro",
            "provenance",
            "export",
            "--project-root",
            "/tmp/project",
            "--token-file",
            "/tmp/token",
        ]);
        assert!(matches!(cli.command, BroCommand::Provenance(_)));
    }

    #[test]
    fn clap_routes_checkout_local_blame() {
        let cli = BroCli::parse_from([
            "bro",
            "blame",
            "--token-file",
            "/tmp/token",
            "--file",
            "src/lib.rs",
            "--line",
            "7",
        ]);
        assert!(matches!(cli.command, BroCommand::Blame(_)));
    }

    #[test]
    fn clap_routes_blame_overlap_to_an_explicit_legacy_daemon() {
        let cli = BroCli::parse_from([
            "bro",
            "blame",
            "--token-file",
            "/tmp/token",
            "--file",
            "src/lib.rs",
            "--line",
            "7",
            "--verify-overlap",
            "--legacy-daemon-url",
            "http://127.0.0.1:17265",
        ]);
        assert!(matches!(cli.command, BroCommand::Blame(_)));
    }
}

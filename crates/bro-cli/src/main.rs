//! Headless client for daemon MCP calls, guidance rendering and event tailing.

use std::io::{self, Write};
use std::time::Duration;

use clap::{Args, Parser, Subcommand};

mod mcp_call;
mod render_global;

#[derive(Default, Debug, Clone)]
struct TailSelectors {
    bros: Vec<String>,
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
}

#[derive(Debug, Clone, Args)]
#[command(
    after_help = "Prints one SSE data payload per stdout line. Selectors are unioned and each flag is repeatable.\n\nExamples:\n  bro tail reviewer writer\n  bro tail --bro reviewer --bro qa\n  bro tail --session <uuid>\n  bro tail --provider codex"
)]
struct TailArgs {
    /// Specific bros to include, by the brofile name they were dispatched with.
    #[arg(long = "bro", value_name = "NAME")]
    bros: Vec<String>,
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
        .default_headers(bro_fleet_client::bearer_headers()?)
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

fn main() -> anyhow::Result<()> {
    restore_default_sigpipe();
    let cli = BroCli::parse();
    let rt = tokio::runtime::Runtime::new()?;

    let result = match cli.command {
        BroCommand::Tail(args) => rt.block_on(run_tail_stream_printer(TailSelectors::from(args))),
        BroCommand::Mcp(args) => rt.block_on(mcp_call::run(args)),
        BroCommand::Render(args) => rt.block_on(render_global::run(args)),
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
            "--bro",
            "solo",
            "--session",
            "sid-123",
            "--provider",
            "codex",
        ]);

        let BroCommand::Tail(args) = cli.command else {
            panic!("expected Tail command");
        };
        let sel = TailSelectors::from(args);
        assert_eq!(sel.bros, vec!["solo", "alpha", "beta"]);
        assert_eq!(sel.sessions, vec!["sid-123"]);
        assert_eq!(sel.providers, vec!["codex"]);
    }
}

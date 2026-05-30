//! `bro-harness` — Anthropic-shaped headless coding agent.
//!
//! Spawned as a subprocess by the daemon exactly like `claude`. Speaks the
//! Anthropic Messages API directly (clean request body — no schema-violating
//! CLI scaffolding), runs its own tool-calling loop, and emits the Claude
//! stream-json envelope on stdout.
//!
//! INVARIANT: stdout is the protocol channel — only NDJSON protocol lines go
//! there. All diagnostics go to stderr.

mod agent_loop;
mod bound;
mod cli;
mod compaction;
mod emit;
mod hooks;
mod report;
mod mcp;
mod registry;
mod session;
mod transport;

use clap::Parser;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = cli::Cli::parse();
    if let Err(e) = agent_loop::run(cli).await {
        // Surface the failure on stderr; the daemon captures it as the task's
        // stderr and marks the task failed on non-zero exit.
        tracing::error!("harness error: {e:#}");
        std::process::exit(1);
    }
}

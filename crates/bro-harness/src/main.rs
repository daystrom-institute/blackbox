//! `bro-harness` — headless coding agent.
//!
//! Spawned as a subprocess by the daemon today; the same logic is also exposed
//! as the `bro_harness` library for in-process execution.
//!
//! INVARIANT: stdout is the protocol channel — only NDJSON protocol lines go
//! there. All diagnostics go to stderr.

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

    let cli = bro_harness::cli::Cli::parse();
    if cli.worker_probe {
        if let Err(e) = bro_harness::worker::run_probe(&cli).await {
            tracing::error!("worker probe failed: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(e) = bro_harness::agent_loop::run(cli).await {
        // Surface the failure on stderr; the daemon captures it as the task's
        // stderr and marks the task failed on non-zero exit.
        tracing::error!("harness error: {e:#}");
        std::process::exit(1);
    }
}

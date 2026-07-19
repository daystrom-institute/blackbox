//! `bro-harness` — headless coding agent.
//!
//! Spawned as an independent subprocess by the daemon. The library target
//! remains available for harness-owned tests and embedding outside blackboxd;
//! the daemon does not link it.
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
    if let Err(e) = bro_harness::agent_loop::run(cli).await {
        // Surface the failure on stderr; the daemon captures it as the task's
        // stderr and marks the task failed on non-zero exit.
        tracing::error!("harness error: {e:#}");
        std::process::exit(1);
    }
}

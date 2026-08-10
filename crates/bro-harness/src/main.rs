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
    if let Err(error) = bro_harness::worker_local_env::materialize_process_env() {
        eprintln!("harness worker-local environment error: {error:#}");
        std::process::exit(1);
    }

    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    // rmcp's reqwest 0.13 rides the rustls no-provider variant (workspace
    // `reqwest-tls-no-provider`), so its client builder panics "No provider
    // set" unless a process-default CryptoProvider exists. The daemon installs
    // ring at its own startup, but the harness is a separate process since the
    // process-boundary extraction; install ring here too. Idempotent.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let cli = bro_harness::cli::Cli::parse();
    match bro_harness::agent_loop::run(cli).await {
        Ok(()) => {
            // Exit explicitly rather than returning into the `#[tokio::main]`
            // runtime drop. `tokio::io::stdin()` parks a blocking read on the
            // blocking pool, and the daemon holds the child's stdin open for
            // the session's whole life, so that read never sees EOF. Dropping
            // the runtime joins the blocking pool, which would then hang
            // forever waiting on that parked read, so the child never exits
            // and the daemon never observes the terminal state. The session
            // snapshot and event log are already durably flushed at the turn
            // boundary before `run` returns, so an immediate exit loses
            // nothing. Symmetric with the error arm below.
            std::process::exit(0);
        }
        Err(e) => {
            // Surface the failure on stderr; the daemon captures it as the
            // task's stderr and marks the task failed on non-zero exit. Use a
            // direct write because fleetd's inherited RUST_LOG may contain a
            // target-only filter that excludes bro-harness diagnostics.
            eprintln!("harness error: {e:#}");
            std::process::exit(1);
        }
    }
}

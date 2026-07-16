//! `bbox-collector` binary: a thin wrapper that loads config, wires shutdown
//! signals, and hands off to `bbox_collector::run`. All logic lives in the lib
//! so tests can exercise it without spawning a process.

use std::path::PathBuf;
use std::sync::Arc;

use bbox_collector::CollectorConfig;
use tokio::sync::Notify;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config_path = config_path();
    tracing::info!(path = %config_path.display(), "loading collector config");
    let config = CollectorConfig::load(&config_path)?;

    let shutdown = Arc::new(Notify::new());
    spawn_shutdown_signal(shutdown.clone());

    bbox_collector::run(config, shutdown).await
}

/// Config path precedence: `--config <path>`, then `BBOX_COLLECTOR_CONFIG`,
/// then the default under the XDG config dir.
fn config_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" | "-c" => {
                if let Some(path) = args.next() {
                    return PathBuf::from(path);
                }
            }
            other if other.starts_with("--config=") => {
                return PathBuf::from(&other["--config=".len()..]);
            }
            _ => {}
        }
    }
    if let Ok(path) = std::env::var("BBOX_COLLECTOR_CONFIG")
        && !path.is_empty()
    {
        return PathBuf::from(path);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("blackbox")
        .join("collector.toml")
}

fn spawn_shutdown_signal(shutdown: Arc<Notify>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(terminate) => terminate,
                    Err(error) => {
                        tracing::warn!(%error, "failed to install SIGTERM handler");
                        return;
                    }
                };
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = terminate.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        shutdown.notify_waiters();
    });
}

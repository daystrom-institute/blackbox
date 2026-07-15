use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use blackbox_corpus_service::{CorpusService, CorpusServicePaths, serve};
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

#[derive(Debug, Parser)]
#[command(
    name = "blackbox-corpusd",
    version,
    about = "Dependency-clean Blackbox FDR and corpus service"
)]
struct Args {
    #[arg(long, env = "BBOX_BIND", default_value = "127.0.0.1")]
    bind: String,

    #[arg(long, env = "BBOX_PORT", default_value_t = 7264)]
    port: u16,

    #[arg(long, env = "TRANSCRIPT_SEARCH_INDEX_PATH")]
    index_path: Option<PathBuf>,

    #[arg(long, env = "BRO_HOME")]
    record_root: Option<PathBuf>,

    #[arg(long, env = "BLACKBOX_FLEET_TRANSCRIPT_ROOT")]
    fleet_transcript_root: Option<PathBuf>,

    #[arg(long, env = "BLACKBOX_SERVICE_TOKEN_FILE")]
    service_token_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();
    let home = dirs::home_dir().context("cannot resolve home directory")?;
    let state_root = std::env::var_os("BLACKBOX_STATE_DIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::state_dir()
                .unwrap_or_else(|| home.join(".local/state"))
                .join("blackbox")
        });
    let record_root = args.record_root.unwrap_or_else(|| state_root.join("bro"));
    let index_path = args.index_path.unwrap_or_else(|| {
        dirs::data_dir()
            .unwrap_or_else(|| home.join(".local/share"))
            .join("blackbox/index")
    });
    let address: SocketAddr = format!("{}:{}", args.bind, args.port)
        .parse()
        .context("invalid blackbox corpus bind address")?;
    if !address.ip().is_loopback() {
        anyhow::bail!(
            "blackbox corpus internal endpoints are local-only; bind to a loopback address"
        );
    }

    let listener = tokio::net::TcpListener::bind(address)
        .await
        .with_context(|| format!("binding blackbox corpus service at {address}"))?;
    let transcript_root = args
        .fleet_transcript_root
        .unwrap_or_else(|| home.join(".local/state/blackbox/fleetd").join("workers"));
    let mut paths = CorpusServicePaths::new(index_path, record_root)
        .with_transcript_roots(vec![transcript_root]);
    if let Some(service_token_file) = args.service_token_file {
        paths = paths.with_service_token_path(service_token_file);
    }
    let service = Arc::new(CorpusService::open(paths)?);
    tracing::info!(address = %address, "blackbox corpus service ready");

    let shutdown = CancellationToken::new();
    spawn_shutdown_signal(shutdown.clone());
    serve(listener, service, shutdown).await
}

fn spawn_shutdown_signal(shutdown: CancellationToken) {
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            let mut terminate =
                match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                    Ok(signal) => signal,
                    Err(error) => {
                        tracing::warn!(%error, "failed to install SIGTERM handler");
                        let _ = tokio::signal::ctrl_c().await;
                        shutdown.cancel();
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
        shutdown.cancel();
    });
}

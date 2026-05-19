use super::SharedState;
use crate::{config, embed_queue, vectors};
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

pub(super) async fn serve_until_shutdown(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    shared: Arc<SharedState>,
    store_dir: PathBuf,
    ct: CancellationToken,
    shutdown_grace_secs: u64,
) -> anyhow::Result<()> {
    spawn_config_reload_handler(shared.clone());
    spawn_shutdown_signal_handler(ct.clone());
    serve_with_grace_period(listener, app, &ct, shutdown_grace_secs).await?;
    persist_shutdown_state(shared, store_dir);
    tracing::info!("blackboxd shut down");
    Ok(())
}

#[cfg(unix)]
fn spawn_config_reload_handler(shared: Arc<SharedState>) {
    tokio::spawn(async move {
        let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
            .expect("install SIGHUP handler");
        loop {
            let _ = sighup.recv().await;
            match config::load() {
                Ok(new_cfg) => {
                    let old_cfg = shared.config.read();
                    if old_cfg.daemon.port != new_cfg.daemon.port
                        || old_cfg.daemon.bind != new_cfg.daemon.bind
                    {
                        tracing::warn!("SIGHUP reload changed bind/port; requires daemon restart");
                    }
                    drop(old_cfg);
                    *shared.config.write() = new_cfg;
                }
                Err(e) => {
                    tracing::warn!("SIGHUP reload failed: {e}");
                }
            }
        }
    });
}

#[cfg(not(unix))]
fn spawn_config_reload_handler(_shared: Arc<SharedState>) {
    tokio::spawn(async {});
}

fn spawn_shutdown_signal_handler(ct: CancellationToken) {
    tokio::spawn(async move {
        // Wait for either Ctrl-C (interactive) or SIGTERM (systemd stop).
        #[cfg(unix)]
        {
            let mut sigterm =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("install SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        ct.cancel();
    });
}

async fn serve_with_grace_period(
    listener: tokio::net::TcpListener,
    app: axum::Router,
    ct: &CancellationToken,
    shutdown_grace_secs: u64,
) -> anyhow::Result<()> {
    let shutdown_grace = std::time::Duration::from_secs(shutdown_grace_secs);
    let graceful_ct = ct.clone();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        graceful_ct.cancelled().await;
    });
    tokio::select! {
        result = server => result?,
        _ = async {
            ct.cancelled().await;
            tokio::time::sleep(shutdown_grace).await;
        } => {
            tracing::warn!(
                grace_secs = shutdown_grace.as_secs(),
                "HTTP graceful shutdown timed out; forcing daemon shutdown path"
            );
        }
    }
    Ok(())
}

fn persist_shutdown_state(shared: Arc<SharedState>, store_dir: PathBuf) {
    embed_queue::shutdown();
    // Tear down long-lived LSP sessions before persistence so JDTLS and friends
    // get a chance to write workspace caches and exit cleanly.
    shared.lsp_sessions.shutdown_all();
    shared.task_store.read().persist(&store_dir);
    flush_vectors_with_timeout();
}

fn flush_vectors_with_timeout() {
    let flush_handle = std::thread::spawn(|| vectors::global().flush_all());
    let flush_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while std::time::Instant::now() < flush_deadline {
        if flush_handle.is_finished() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    if flush_handle.is_finished() {
        if let Err(err) = flush_handle.join().expect("flush thread panic") {
            tracing::warn!(error = %err, "vector partition force-flush on shutdown failed");
        }
    } else {
        tracing::warn!(
            "vector partition force-flush on shutdown timed out after 5s; \
             next start will rebuild derived files from WAL"
        );
    }
}

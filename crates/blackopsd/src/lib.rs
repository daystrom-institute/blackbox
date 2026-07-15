mod authority_actor;
mod catalog;
mod clients;
mod config;
mod error;
mod mcp;
mod runtime;
mod service;

use std::sync::Arc;

pub use authority_actor::AuthorityActor;
pub use catalog::{CatalogImportReport, import_catalog};
pub use clients::{BlackboxRecordHttpClient, FleetControlCapability, FleetHttpClient};
pub use config::BlackopsdConfig;
pub use error::{BlackopsdError, BlackopsdResult};
pub use runtime::{
    BlackopsRuntime, ExecutionProfile, ReconcileReport, RuntimeStatus, SessionAgentCapability,
    SessionAtomCapability,
};
pub use service::{AgentCall, AgentListCall, RoutedCapabilityRequest, router};

pub const BUILD_ID: &str = env!("BLACKOPSD_BUILD_ID");

pub async fn run(config: BlackopsdConfig) -> anyhow::Result<()> {
    let config = config.normalized()?;
    let service_token = Arc::new(
        bro_rpc::ServiceToken::load_or_create(config.service_token_path())
            .map_err(|error| anyhow::anyhow!("loading blackops service token: {error}"))?,
    );
    let fleet = Arc::new(
        FleetHttpClient::new(
            config.fleetd_url.clone(),
            config.upstream_timeout(),
            service_token.clone(),
        )
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?,
    );
    let records = Arc::new(
        BlackboxRecordHttpClient::new(
            config.blackboxd_url.clone(),
            config.upstream_timeout(),
            service_token.clone(),
        )
        .map_err(|error| anyhow::anyhow!("{}: {}", error.code, error.message))?,
    );
    let runtime = BlackopsRuntime::open(
        config.state_dir.clone(),
        fleet.clone(),
        fleet,
        records,
        ExecutionProfile {
            provider: config.provider()?,
            model: config.default_model.clone(),
        },
        BUILD_ID,
    )
    .await?;
    let catalog = catalog::import_catalog(&runtime.authority(), config.catalog_path()).await?;
    tracing::info!(
        shipped_atoms = catalog.shipped_atoms,
        installed_atoms = catalog.installed_atoms,
        definitions = catalog.definitions,
        "blackops catalog imported"
    );
    let reconciler = runtime.clone();
    let interval = config.reconcile_interval();
    let reconcile_task = tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            let report = reconciler.drive_once().await;
            if !report.errors.is_empty() {
                tracing::warn!(errors = ?report.errors, "blackops reconciliation degraded");
            }
        }
    });
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    tracing::info!(
        address = %config.bind,
        build_id = BUILD_ID,
        "blackopsd operational authority listening"
    );
    axum::serve(listener, router(runtime.clone(), service_token))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    reconcile_task.abort();
    let _ = reconcile_task.await;
    runtime.authority().shutdown().await;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = terminate.recv() => {}
                }
            }
            Err(error) => {
                tracing::warn!(%error, "failed to install SIGTERM handler");
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

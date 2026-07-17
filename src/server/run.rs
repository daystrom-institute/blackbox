use super::RuntimeRole;
use super::background::start_background_tasks;
use super::mcp::build_http_app;
use super::open::open_shared_state;
use super::shutdown::serve_until_shutdown;
use super::startup::init_logging;
use crate::util;
use tokio_util::sync::CancellationToken;

pub async fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    let migrated = util::migrate_legacy_defaults(&home)?;
    let telemetry_guard = init_logging(&home, migrated);

    let runtime_role = RuntimeRole::from_env()?;
    let opened = open_shared_state(&home, runtime_role)?;
    let cfg = opened.cfg;
    let shared = opened.shared;
    let store_dir = opened.store_dir;
    let bind_host = opened.bind_host;
    let bind_is_loopback = opened.bind_is_loopback;
    if runtime_role == RuntimeRole::Corpus
        && !bind_is_loopback
        && !cfg.daemon.allow_nonloopback_bind
    {
        anyhow::bail!(
            "blackboxd corpus role exposes private capability and record endpoints and must bind to loopback; set BBOX_ALLOW_NONLOOPBACK_BIND=1 (daemon.allow_nonloopback_bind) to opt in for containerized/cluster deployment behind a trusted ingress"
        );
    }
    start_background_tasks(shared.clone(), runtime_role).await?;

    // MCP service
    let port = cfg.daemon.port;

    let ct = CancellationToken::new();
    let app = build_http_app(shared.clone(), &cfg, &ct, runtime_role);

    // Bind address resolved above (hoisted so SharedState gets the
    // loopback flag). Default `127.0.0.1`; BBOX_BIND=0.0.0.0 opens
    // the listener to docker-bridged peers — closed-network only.
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;
    tracing::info!(
        role = runtime_role.as_str(),
        "blackboxd listening on http://{bind_host}:{port}/mcp (loopback={bind_is_loopback})"
    );

    serve_until_shutdown(
        listener,
        app,
        shared,
        store_dir,
        ct,
        cfg.daemon.shutdown_grace_secs,
        runtime_role,
        telemetry_guard,
    )
    .await
}

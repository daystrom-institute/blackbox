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
    init_logging(&home, migrated);

    let opened = open_shared_state(&home)?;
    let cfg = opened.cfg;
    let shared = opened.shared;
    let store_dir = opened.store_dir;
    let bind_host = opened.bind_host;
    let bind_is_loopback = opened.bind_is_loopback;
    // Select the harness executor BEFORE anything can dispatch. Default is
    // fleetd, so a worker outlives this daemon process; `BLACKBOX_EXECUTOR=local`
    // (or `daemon.executor = "local"`) is the explicit escape back to
    // daemon-child workers. Installing here also arms fleetd re-adoption: the
    // first connection re-attaches whatever survived our restart.
    crate::orchestration::install_harness_executor(
        cfg.daemon.executor,
        store_dir.clone(),
        shared.task_store.clone(),
        shared.tail_tx.clone(),
        Some(shared.system_events.clone()),
    );

    // Bind before starting any background work that probes registered project
    // paths. A stalled mount must not prevent the daemon from claiming its
    // listener; the initial lifecycle pass itself is spawned below.
    let port = cfg.daemon.port;
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;

    start_background_tasks(shared.clone()).await?;

    // MCP service
    let ct = CancellationToken::new();
    let app = build_http_app(shared.clone(), &cfg, &ct);

    // Bind address resolved above (hoisted so SharedState gets the
    // loopback flag). Default `127.0.0.1`; BBOX_BIND=0.0.0.0 opens
    // the listener to docker-bridged peers — closed-network only.
    tracing::info!(
        "blackboxd listening on http://{bind_host}:{port}/mcp (loopback={bind_is_loopback})"
    );

    serve_until_shutdown(
        listener,
        app,
        shared,
        store_dir,
        ct,
        cfg.daemon.shutdown_grace_secs,
    )
    .await
}

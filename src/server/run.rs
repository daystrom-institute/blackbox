use super::background::start_background_tasks;
use super::instance_lock::acquire_instance_locks;
use super::mcp::build_http_app;
use super::open::open_shared_state;
use super::shutdown::serve_until_shutdown;
use super::startup::init_logging;
use tokio_util::sync::CancellationToken;

pub async fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    // Load once: the claim below and the store opens further down must agree
    // on which roots this daemon owns, and a reload between them could not
    // guarantee that.
    let loaded = crate::config::load()?;
    let roots = super::instance_lock::instance_lock_roots(&loaded);
    // Claim every root this daemon writes to before anything creates a
    // directory or opens a shared log: a process that loses the claim returns
    // from here having opened nothing.
    let instance_locks = acquire_instance_locks(&roots)?;
    init_logging(&home);

    let opened = open_shared_state(&home, loaded, &instance_locks)?;
    // R31F1: the single-writer claim outlives every store opened under it.
    // Binding it here keeps it held until `run` returns; process exit by any
    // other route releases it with the file descriptions.
    let _instance_locks = instance_locks;
    let cfg = opened.cfg;
    let shared = opened.shared;
    let store_dir = opened.store_dir;
    let bind_host = opened.bind_host;
    let bind_is_loopback = opened.bind_is_loopback;
    // Select the harness executor BEFORE anything can dispatch. Default is
    // fleetd, so a worker outlives this daemon process; `BLACKBOX_EXECUTOR=local`
    // (or `daemon.executor = "local"`) is the explicit escape back to
    // daemon-child workers. Installing here also arms fleetd re-adoption, which
    // starts once the listener is bound and background state is restored.
    crate::orchestration::install_configured_harness_executor(
        cfg.daemon.executor,
        store_dir.clone(),
        cfg.daemon.fleetd_endpoint.as_deref(),
        cfg.daemon.fleetd_token_file.as_deref(),
        cfg.daemon.fleetd_worker_home.as_deref(),
        cfg.daemon.fleetd_worker_bro_home.as_deref(),
        cfg.daemon.fleetd_tls_fingerprint.as_deref(),
        shared.task_store.clone(),
        shared.tail_tx.clone(),
        Some(std::sync::Arc::new(
            crate::server::knowledge_source::DaemonWorkspaceBindingAuthority::new(shared.clone()),
        )),
    )?;

    // Bind before starting any background work that probes registered project
    // paths. A stalled mount must not prevent the daemon from claiming its
    // listener; the initial lifecycle pass itself is spawned below.
    let port = cfg.daemon.port;
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;

    start_background_tasks(shared.clone()).await?;
    // Dial fleetd and re-attach whatever survived our restart now, rather than
    // on the first dispatch. Never blocks or fails startup.
    crate::orchestration::start_harness_readoption();

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

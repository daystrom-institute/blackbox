use super::restore::restore_runtime_state;
use super::startup::{
    configure_dispatch_mcp_env, discover_transcript_roots, init_logging, resolve_codex_root,
};
use super::*;
use crate::*;
use parking_lot::RwLock;
use std::sync::Arc;

pub async fn run() -> anyhow::Result<()> {
    let home = dirs::home_dir().expect("cannot determine home directory");
    let migrated = util::migrate_legacy_defaults(&home)?;
    init_logging(&home, migrated);

    // Load configuration
    let cfg = config::load()?;
    let cfg_arc = Arc::new(RwLock::new(cfg.clone()));

    // Transcript index roots - from config or env
    let roots = discover_transcript_roots(&cfg, &home);
    let codex_root = resolve_codex_root(&cfg, &home);

    let index_path = cfg.paths.index_path.clone();

    tracing::info!(
        "Roots: {:?}",
        roots
            .iter()
            .map(|(n, p)| format!("{n}={}", p.display()))
            .collect::<Vec<_>>()
    );
    if let Some(ref cr) = codex_root {
        tracing::info!("Codex root: {}", cr.display());
    }
    tracing::info!("Index path: {}", index_path.display());

    let projects_path = cfg.paths.projects_path.clone();
    let kb_path = cfg.paths.knowledge_path.clone();
    let th_path = cfg.paths.threads_path.clone();
    let rm_path = cfg.paths.roadmap_path.clone();
    let mut idx = TranscriptIndex::open_or_create(
        &index_path,
        roots,
        codex_root,
        projects_path.clone(),
        kb_path.clone(),
        th_path.clone(),
        rm_path.clone(),
    )?;
    let projects_store = ProjectRegistry::open(&projects_path)?;
    tracing::info!("Project registry: {}", projects_path.display());

    let mut kb = Knowledge::open(&kb_path)?;
    tracing::info!("Knowledge store: {}", kb_path.display());

    // Sync the auto-generated tool reference into the knowledge store
    // so every agent's global memory picks up the current tool surface
    // on the next render. Idempotent: no-op when content is unchanged.
    match tool_docs::sync_into_knowledge(&mut kb) {
        Ok(r) if r.wrote => tracing::info!("Tool reference synced ({} bytes)", r.bytes),
        Ok(_) => tracing::debug!("Tool reference already up to date"),
        Err(e) => tracing::warn!("Tool reference sync failed: {e:#}"),
    }

    // Load system memory catalog from disk before SharedState construction.
    // Fails closed if the defaults directory is missing or any file is malformed.
    {
        let memory_ctx = serde_json::json!({
            "version": env!("CARGO_PKG_VERSION"),
            "mcp_name": &cfg.daemon.mcp_name,
        });
        system_memory::init(
            &cfg.paths.defaults_memories_dir,
            cfg.paths.user_memories_dir.as_deref(),
            &memory_ctx,
        )?;
        tracing::info!(
            "System memory catalog loaded from {}",
            cfg.paths.defaults_memories_dir.display()
        );
    }

    configure_dispatch_mcp_env(&cfg);

    // Sweep orphaned Gemini policy tempfiles from crashed/force-killed
    // dispatches. Files younger than 24h are kept in case they belong
    // to live tasks.
    match orchestration::mcp::sweep_stale_gemini_policies(24) {
        Ok(n) if n > 0 => tracing::info!("swept {n} stale gemini policy file(s)"),
        Ok(_) => {}
        Err(e) => tracing::debug!("gemini policy sweep: {e:#}"),
    }

    let th = Threads::open(&th_path)?;
    tracing::info!("Thread store: {}", th_path.display());
    if let Err(err) = idx.index_threads_store(&th) {
        tracing::warn!(error = %err, "thread index sync failed; will retry on next reindex cycle");
    }

    let roadmap_store = Roadmap::open(&rm_path)?;
    tracing::info!("Roadmap store: {}", rm_path.display());

    let notes_path = cfg.paths.notes_path.clone();
    let notes_store = Notes::open(&notes_path)?;
    tracing::info!("Notes store: {}", notes_path.display());

    let pins_path = cfg.paths.pins_path.clone();
    let pins_store = Pins::open(&pins_path)?;
    tracing::info!("Pins store: {}", pins_path.display());

    let packets_dir = cfg.paths.packets_dir.clone();
    let packets_store = Packets::open(&packets_dir)?;
    tracing::info!("Packets store: {}", packets_dir.display());

    let artifacts_dir = cfg.paths.artifacts_dir.clone();
    let agent_adapter_registry = Arc::new(RwLock::new(
        orchestration::agents::adapter::AgentAdapterRegistry::new(),
    ));
    let artifacts_store = artifacts::ArtifactCatalog::open(&artifacts_dir)?;
    tracing::info!("Artifact catalog: {}", artifacts_store.root().display());
    match artifacts_store.backfill_content_hashes() {
        Ok(r) => {
            if r.active_updated > 0 || r.version_updated > 0 || r.missing_artifacts > 0 {
                tracing::info!(
                    "Artifact hash backfill: {} active updated, {} version updated, {} missing payloads",
                    r.active_updated,
                    r.version_updated,
                    r.missing_artifacts
                );
            }
        }
        Err(e) => tracing::warn!("Artifact hash backfill failed: {e:#}"),
    }

    // Orchestration state
    let store_dir = cfg.paths.bro_home.clone();
    let task_ttl = cfg.daemon.task_ttl_ms;
    let task_store = TaskStore::load(&store_dir, task_ttl);
    let badgey_proposals = Arc::new(orchestration::badgey::ProposalStore::new(
        store_dir.clone(),
    )?);
    let badgey_journal = Arc::new(orchestration::badgey::ActionJournal::new(
        store_dir.clone(),
    )?);

    let (tail_tx, _) = broadcast::channel::<TailEvent>(1024);

    // Spawn background reindex thread
    let reindex_interval = cfg.index.reindex_interval_secs;
    index::spawn_reindex_thread(
        idx.index_handle(),
        idx.reindex_config(),
        idx.field_handles(),
        std::time::Duration::from_secs(reindex_interval),
    );

    // Bind address resolution is hoisted here so SharedState carries
    // a definitive `bind_is_loopback` flag; the listener bind below
    // uses the same value. Default 127.0.0.1; BBOX_BIND=0.0.0.0 to
    // accept docker-bridged webhooks.
    let bind_host = cfg.daemon.bind.clone();
    let bind_is_loopback = is_loopback_bind(&bind_host);

    let edge_index = if cfg.index.edge_index_boot_rebuild {
        edge_index::EdgeIndex::rebuild(&edge_index::EdgeStoreRefs {
            index: &idx,
            knowledge: &kb,
            threads: &th,
            notes: &notes_store,
            task_store: &task_store,
            roadmap: &roadmap_store,
            edges_dir: edge_index::edges_dir_from_bro_store(&store_dir),
            registered_project_ids: Some(
                projects_store
                    .list()
                    .into_iter()
                    .map(|project| project.project_id)
                    .collect(),
            ),
            include_tantivy_projection: false,
            include_observed: true,
        })
    } else {
        tracing::info!(
            "startup EdgeIndex rebuild deferred (set BLACKBOX_EDGE_INDEX_BOOT_REBUILD=1 to restore eager rebuild)"
        );
        edge_index::EdgeIndex::default()
    };

    let shared = Arc::new(SharedState {
        idx: RwLock::new(idx),
        kb: RwLock::new(kb),
        roadmap: RwLock::new(roadmap_store),
        threads: RwLock::new(th),
        notes: RwLock::new(notes_store),
        pins: RwLock::new(pins_store),
        projects: RwLock::new(projects_store),
        packets: RwLock::new(packets_store),
        artifacts: RwLock::new(artifacts_store),
        bbox_watcher: std::sync::Mutex::new(None),
        edge_index: RwLock::new(edge_index),
        path_cache: RwLock::new(path_cache::PathCache::default()),
        task_store: Arc::new(RwLock::new(task_store)),
        tail_tx: tail_tx.clone(),
        store_dir: store_dir.clone(),
        running_arcs: RwLock::new(HashMap::new()),
        wait_store: Arc::new(crate::workflow::wait::WaitStore::new()),
        webhooks: Arc::new(webhooks::WebhookRegistry::new()),
        pollers: Arc::new(pollers::PollerRegistry::new()),
        crons: Arc::new(crons::CronRegistry::new()),
        whiteboards: Arc::new(whiteboards::WhiteboardRegistry::new()),
        workflow_registry: Arc::new(RwLock::new(HashMap::new())),
        bind_is_loopback,
        signal_log: RwLock::new(std::collections::VecDeque::with_capacity(SIGNAL_LOG_CAP)),
        webhook_delivery_log: RwLock::new(std::collections::VecDeque::with_capacity(
            WEBHOOK_LOG_CAP,
        )),
        arc_cancel_tokens: RwLock::new(HashMap::new()),
        councils: Arc::new(council::CouncilRegistry::new()),
        resume_leases: Arc::new(orchestration::resume_lease::ResumeLeaseRegistry::new()),
        agent_adapter_registry: agent_adapter_registry.clone(),
        badgey_registry: Arc::new(orchestration::badgey::BadgeyRegistry::new()),
        badgey_proposals,
        badgey_journal,
        slack_thread_store: Arc::new(
            slack_thread_store::SlackThreadStore::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack thread store at {store_dir:?}: {e}")),
        ),
        slack_channel_bindings: Arc::new(
            slack_channel_bindings::SlackChannelBindings::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack channel bindings at {store_dir:?}: {e}")),
        ),
        slack_proposal_links: Arc::new(
            slack_proposal_links::SlackProposalLinks::open(&store_dir)
                .unwrap_or_else(|e| panic!("opening slack proposal links at {store_dir:?}: {e}")),
        ),
        lsp_sessions: lsp::LspSessionManager::with_lsp_config(&cfg.lsp),
        config: cfg_arc.clone(),
        atom_invocation_store: Arc::new(RwLock::new(
            orchestration::atoms::invocation::InvocationStore::new(
                store_dir.join("atom-invocations.json"),
            ),
        )),
        vector_store: Arc::new(
            vectors::VectorStore::open_unloaded(vectors::default_vectors_dir())
                .expect("default vector store placeholder should open"),
        ),
        system_events: Arc::new(system_events::EventHub::new(
            system_events::EventStore::new(&store_dir),
            system_events::OutboxStore::new(store_dir.join("events").join("outbox"))
                .unwrap_or_else(|e| panic!("opening outbox store at {store_dir:?}: {e}")),
            store_dir.join("reactions"),
            store_dir.join("identities"),
        )),
    });
    shared
        .agent_adapter_registry
        .write()
        .register(Arc::new(BadgeyAgentAdapter {
            state: shared.clone(),
        }));
    restore_badgey_registry_from_notes(&shared);
    recover_badgey_non_terminal_state(&shared);
    embed_queue::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    embed_queue::install_contradiction_state(shared.clone());
    embed_queue::install(embed::queue::EmbedQueueHandle::start_default_without_store());

    std::thread::Builder::new()
        .name("blackbox-vectors-warmup".into())
        .spawn(|| {
            let started = std::time::Instant::now();
            let store = vectors::global();
            embed_queue::install(embed::queue::EmbedQueueHandle::start_default_with_store(
                store.clone(),
            ));
            tracing::info!(
                partitions = store.partition_count(),
                elapsed_ms = started.elapsed().as_millis(),
                "vector store warmed"
            );
        })
        .map_err(|e| anyhow::anyhow!("spawning vector store warmup thread: {e}"))?;

    // Watch the tantivy corpus and rebuild the EdgeIndex whenever new docs
    // land via the auto-reindex thread (60s poll interval is sufficient
    // since the reindex tick is 120s by default).
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));
    let storage_gc_interval = storage_gc_interval_from_env();
    tracing::info!(
        interval_secs = storage_gc_interval.as_secs(),
        "storage GC maintenance thread enabled"
    );
    spawn_storage_gc_thread(shared.clone(), storage_gc_interval);

    // Task-completed router: subscribe to tail events and forward each
    // TaskCompleted as a `task-completed` signal through the installed
    // routing packet (domain:auto-digest/task-completed-routing). When
    // the packet is not installed the dispatch is a fast no_match
    // dead-letter — no performance impact on normal operation.
    {
        let shared_for_router = shared.clone();
        let mut tail_rx = tail_tx.subscribe();
        tokio::spawn(async move {
            loop {
                match tail_rx.recv().await {
                    Ok(orchestration::tail::TailEvent::TaskCompleted {
                        task_id,
                        source_session,
                        task_kind,
                        ..
                    }) => {
                        let entity = serde_json::json!({
                            "signal": "task-completed",
                            "event_type": "task-completed",
                            "kind": "task-completed",
                            "task_id": task_id,
                            "session_id": source_session,
                            "task_kind": task_kind,
                        });
                        if let Err(e) = dispatch_routed_event(
                            shared_for_router.clone(),
                            "task-completed",
                            "domain:auto-digest/task-completed-routing",
                            entity,
                            None,
                        )
                        .await
                        {
                            tracing::debug!("task-completed router: {e:#}");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("task-completed router: lagged {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // System events are also workflow signals: a Wait on
    // `bro.identity.provisioned` should resume when the durable event is
    // emitted by a reaction. Only dispatch when a matching wait already exists
    // so ordinary event traffic does not fill the signal log with idle entries.
    {
        let shared_for_system_event_signals = shared.clone();
        let mut system_event_rx = shared.system_events.subscribe();
        tokio::spawn(async move {
            loop {
                match system_event_rx.recv().await {
                    Ok(event) => {
                        let signal = event.kind.to_wire().to_string();
                        let has_wait = shared_for_system_event_signals
                            .wait_store
                            .snapshot()
                            .into_iter()
                            .any(|w| w.signal == signal);
                        if !has_wait {
                            continue;
                        }
                        let correlation = event.correlation.clone();
                        let payload = serde_json::to_value(&event).unwrap_or_else(|e| {
                            json!({
                                "event_id": event.id,
                                "kind": signal,
                                "serialization_error": e.to_string(),
                            })
                        });
                        let resolved = crate::server::routes::signal_arc_dispatch(
                            &shared_for_system_event_signals,
                            &signal,
                            correlation,
                            payload,
                        )
                        .await;
                        tracing::debug!(
                            signal,
                            result = %resolved,
                            "system event signal bridge dispatched"
                        );
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("system event signal bridge lagged {n} events");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }

    // Start .bbox/ filesystem watcher for all registered projects.
    {
        let project_roots: Vec<(String, std::path::PathBuf)> = shared
            .projects
            .read()
            .list()
            .into_iter()
            .map(|r| (r.project_id, std::path::PathBuf::from(r.canonical_path)))
            .collect();
        let catalog = Arc::new(shared.artifacts.read().clone());
        match watcher::BbxWatcher::start(project_roots, catalog) {
            Ok(w) => {
                *shared.bbox_watcher.lock().unwrap() = Some(w);
                tracing::info!(".bbox/ artifact watcher started");
            }
            Err(e) => tracing::warn!(".bbox/ artifact watcher failed to start: {e:#}"),
        }
    }

    restore_runtime_state(&shared).await;

    // Startup compaction — drop events older than 7 days / cap at 10k,
    // and drop succeeded outbox records older than 7 days. Failures
    // log and continue; the worker still starts.
    {
        let now = crate::util::now_iso();
        match shared.system_events.compact_with_now(&now) {
            Ok(report)
                if report.event_journal.dropped_by_age > 0
                    || report.event_journal.dropped_by_count > 0
                    || report.outbox.dropped_succeeded > 0 =>
            {
                tracing::info!(
                    "event journal compaction: kept {} (dropped {} by age, {} by count)",
                    report.event_journal.after,
                    report.event_journal.dropped_by_age,
                    report.event_journal.dropped_by_count
                );
                tracing::info!(
                    "outbox compaction: kept {} (dropped {} succeeded)",
                    report.outbox.after,
                    report.outbox.dropped_succeeded
                );
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("system event compaction failed: {e:#}"),
        }
    }

    // Outbox worker — background task that claims due records, evaluates
    // gates, executes supported actions, and marks succeeded/retry/dead-lettered.
    {
        let worker_state = shared.clone();
        tokio::spawn(async move {
            crate::system_events::worker::run_worker(worker_state).await;
        });
    }

    // Packet self-heal scanner — off by default. Walks recent
    // packet events on an interval, flags candidates (high no_match
    // rate, low audit fidelity) by writing `op="repair_candidate"`
    // events. Does NOT dispatch repair agents — that's a separate
    // feature gated behind its own flag (not yet implemented).
    let scanner_config = ScannerConfig::from_env();
    if scanner_config.enabled {
        tracing::info!(
            interval_secs = scanner_config.interval.as_secs(),
            window_hours = scanner_config.window.as_secs() / 3600,
            no_match_threshold = scanner_config.no_match_threshold,
            fidelity_threshold = scanner_config.fidelity_threshold,
            "packet self-heal scanner: enabled"
        );
        let shared_for_scanner = shared.clone();
        tokio::spawn(async move {
            let cfg = scanner_config;
            let mut ticker = tokio::time::interval(cfg.interval);
            // Discard the immediate t=0 tick; run the first pass after
            // one interval so short-interval dev setups don't stampede
            // at startup.
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let result = {
                    let guard = shared_for_scanner.packets.read();
                    guard.scanner_step(&cfg)
                };
                match result {
                    Ok(cands) if !cands.is_empty() => {
                        tracing::info!(
                            flagged = cands.len(),
                            "packet self-heal scanner: flagged repair candidates"
                        );
                    }
                    Ok(_) => {
                        tracing::debug!("packet self-heal scanner: no candidates this tick");
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "packet self-heal scanner: tick failed");
                    }
                }
            }
        });
    } else {
        tracing::debug!("packet self-heal scanner: disabled");
    }

    // MCP service
    let port = cfg.daemon.port;

    let ct = CancellationToken::new();
    let server_config = StreamableHttpServerConfig::default()
        .with_cancellation_token(ct.child_token())
        .with_stateful_mode(true);

    let shared_for_mcp = shared.clone();
    let session_keep_alive = cfg.daemon.mcp_session_keepalive_secs;
    let mut session_manager = LocalSessionManager::default();
    session_manager.session_config.keep_alive =
        Some(std::time::Duration::from_secs(session_keep_alive));
    let mcp_service: StreamableHttpService<BlackboxServer, LocalSessionManager> =
        StreamableHttpService::new(
            move || Ok(BlackboxServer::new(shared_for_mcp.clone())),
            session_manager.into(),
            server_config,
        );

    let app = axum::Router::new()
        .route("/tail", axum::routing::get(tail_handler))
        .route("/roster", axum::routing::get(roster_handler))
        .route("/orchestrate", axum::routing::post(orchestrate_handler))
        .route(
            "/orchestrate/stream",
            axum::routing::post(orchestrate_stream_handler),
        )
        .route(
            "/orchestrate/status",
            axum::routing::get(orchestrate_status_handler),
        )
        .route(
            "/orchestrate/list",
            axum::routing::get(orchestrate_list_handler),
        )
        .route(
            "/orchestrate/peek",
            axum::routing::get(orchestrate_peek_handler),
        )
        .route("/webhook/{name}", axum::routing::post(webhook_handler))
        .route(
            "/webhook/{name}/replay",
            axum::routing::post(webhook_replay_handler),
        )
        .route(
            "/orchestrate/by-id",
            axum::routing::post(orchestrate_by_id_handler),
        )
        .route("/irc/exec", axum::routing::post(irc_exec_handler))
        .route("/irc/resume", axum::routing::post(irc_resume_handler))
        .route("/irc/broadcast", axum::routing::post(irc_broadcast_handler))
        .route(
            "/irc/status/{task_id}",
            axum::routing::get(irc_status_handler),
        )
        .route("/irc/dashboard", axum::routing::get(irc_dashboard_handler))
        .route("/irc/cancel", axum::routing::post(irc_cancel_handler))
        .route(
            "/irc/team/{team_name}",
            axum::routing::get(irc_team_handler),
        )
        .route(
            "/admin/packet/compile",
            axum::routing::post(admin_packet_compile),
        )
        .route(
            "/admin/workflow/install",
            axum::routing::post(admin_workflow_install),
        )
        .route(
            "/admin/artifact/install",
            axum::routing::post(admin_artifact_install),
        )
        .route(
            "/admin/artifact/list",
            axum::routing::get(admin_artifact_list),
        )
        .route(
            "/admin/artifact/supersede",
            axum::routing::post(admin_artifact_supersede),
        )
        .route(
            "/admin/artifact/remove",
            axum::routing::post(admin_artifact_remove),
        )
        .route(
            "/admin/webhook/install",
            axum::routing::post(admin_webhook_install),
        )
        .route(
            "/admin/poller/install",
            axum::routing::post(admin_poller_install),
        )
        .route(
            "/admin/cron/install",
            axum::routing::post(admin_cron_install),
        )
        .route(
            "/admin/brofile/upsert",
            axum::routing::post(admin_brofile_upsert),
        )
        .route("/admin/team/upsert", axum::routing::post(admin_team_upsert))
        .route(
            "/council",
            axum::routing::post(council::http::create).get(council::http::list),
        )
        .route(
            "/council/{id}",
            axum::routing::get(council::http::open).delete(council::http::close),
        )
        .route(
            "/council/{id}/post",
            axum::routing::post(council::http::post),
        )
        .route(
            "/council/{id}/tail",
            axum::routing::get(council::http::tail),
        )
        .with_state(shared.clone())
        .nest_service("/mcp", mcp_service);

    // Bind address resolved above (hoisted so SharedState gets the
    // loopback flag). Default `127.0.0.1`; BBOX_BIND=0.0.0.0 opens
    // the listener to docker-bridged peers — closed-network only.
    let listener = tokio::net::TcpListener::bind(format!("{bind_host}:{port}")).await?;
    tracing::info!(
        "blackboxd listening on http://{bind_host}:{port}/mcp (loopback={bind_is_loopback})"
    );

    let shutdown_grace = std::time::Duration::from_secs(cfg.daemon.shutdown_grace_secs);
    let signal_ct = ct.clone();
    #[cfg(unix)]
    {
        let shared_for_hup = shared.clone();
        tokio::spawn(async move {
            let mut sighup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())
                .expect("install SIGHUP handler");
            loop {
                let _ = sighup.recv().await;
                match config::load() {
                    Ok(new_cfg) => {
                        let old_cfg = shared_for_hup.config.read();
                        if old_cfg.daemon.port != new_cfg.daemon.port
                            || old_cfg.daemon.bind != new_cfg.daemon.bind
                        {
                            tracing::warn!(
                                "SIGHUP reload changed bind/port; requires daemon restart"
                            );
                        }
                        drop(old_cfg);
                        *shared_for_hup.config.write() = new_cfg;
                    }
                    Err(e) => {
                        tracing::warn!("SIGHUP reload failed: {e}");
                    }
                }
            }
        });
    }
    #[cfg(not(unix))]
    {
        tokio::spawn(async {});
    }

    tokio::spawn(async move {
        // Wait for either Ctrl-C (interactive) or SIGTERM (systemd
        // stop). Without the SIGTERM branch, `systemctl stop` would
        // not signal graceful shutdown and would rely on the
        // TimeoutStopSec SIGKILL.
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
        signal_ct.cancel();
    });

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

    // Persist tasks on shutdown
    embed_queue::shutdown();
    // Tear down long-lived LSP sessions before persistence so JDTLS
    // and friends get a chance to write their workspace caches and
    // exit cleanly. shutdown_all is best-effort and bounded.
    shared.lsp_sessions.shutdown_all();
    shared.task_store.read().persist(&store_dir);
    // Best-effort vector-partition force-flush with a short timeout.
    // The earlier unconditional `vectors::global().flush_all()` could
    // block here for tens of seconds if any embed worker was holding a
    // partition write lock for a mid-flight voyage batch — long enough
    // to push systemd past TimeoutStopSec=90 and trigger SIGKILL,
    // which is worse than just leaving the WAL to replay on next start.
    // Spawn it on a thread + join with a short cap; if it doesn't
    // finish in time, drop on the floor and exit cleanly. The next
    // daemon start restores from any matching vector snapshot and replays
    // the WAL tail, or falls back to a full WAL rebuild if no usable
    // snapshot exists.
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
        // Detach; the OS reaps it when the process exits.
    }
    tracing::info!("blackboxd shut down");
    Ok(())
}

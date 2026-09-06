use super::restore::restore_runtime_state;
use super::{SharedState, dispatch_routed_event, spawn_edge_index_rebuild_watcher};
use crate::packets::ScannerConfig;
use crate::server::routes::{SignalDispatchOrigin, signal_arc_dispatch};
use crate::server::runtime_metrics::{
    spawn_runtime_metrics_sampler, spawn_scheduler_latency_probe,
};
use crate::server::storage_gc::{spawn_storage_gc_thread, storage_gc_interval_from_env};
use crate::tools::bro_helpers::tier0_cosine_threshold_from_env;
use crate::{embed, embed_queue, orchestration, util, vectors, watcher};
use std::sync::Arc;

pub(super) async fn start_background_tasks(shared: Arc<SharedState>) -> anyhow::Result<()> {
    let runtime_handle = tokio::runtime::Handle::current();
    super::code_source::resume_pending_activations(shared.clone());
    super::code_source::spawn_reconciler(&shared, runtime_handle.clone());
    super::code_source::spawn_scheduler(&shared, runtime_handle.clone());
    super::code_source::spawn_commit_observer(&shared);
    super::code_source::spawn_store_maintenance(&shared)?;
    super::history_activation::spawn_worker(&shared)?;
    super::provenance_import::spawn_worker(&shared)?;
    // Operator-minted workspace bindings are durable: re-arm the ones
    // persisted under the knowledge-source store before anything can capture.
    super::knowledge_source::restore_operator_workspace_bindings(&shared);
    configure_dispatch_path_env();
    configure_embed_runtime(&shared);
    spawn_vector_warmup_thread(shared.clone())?;
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));
    spawn_storage_gc(shared.clone());
    spawn_runtime_metrics_sampler();
    // Off-runtime companion to the sampler above: the sampler is a
    // tokio::spawn and therefore goes blind exactly when the runtime stalls.
    // See design/daemon-runtime/healthz-ingest-starvation.md §6.
    spawn_scheduler_latency_probe(runtime_handle.clone());
    spawn_task_completed_router(shared.clone());
    spawn_system_event_signal_bridge(shared.clone());
    // Replay durable arc checkpoints: re-park Wait-suspended arcs and
    // mark mid-dispatch arcs interrupted. After the signal bridge so a
    // resumed wait's ledger catch-up and live bridge dispatch overlap
    // instead of leaving a gap. AWAITED, not spawned: the pass
    // pre-claims resumable arcs' admission keys and this fn completes
    // before the daemon serves requests, so a fresh StartArc can never
    // steal a checkpointed arc's singleton key in the boot window (the
    // per-arc resumes themselves still run as detached tasks).
    crate::workflow::engine::rehydrate_arcs(shared.clone()).await;
    // Inventory and checkout reconciliation may probe registered paths on
    // stalled mounts. Keep the initial pass off the listener startup path just
    // like subsequent periodic passes.
    tokio::spawn(run_knowledge_lifecycle_pass(shared.clone()));
    // Reconcile the published knowledge index from durable accepted
    // content. A process that died between a pointer swap and its index
    // commit leaves accepted reads on the new generation and search on the
    // old one; nothing else in the daemon closes that gap, because live
    // convergence keeps no durable record. Off the listener startup path,
    // like every other reconciliation pass here.
    tokio::spawn(run_published_index_convergence_pass(shared.clone()));
    start_bbox_watcher(&shared);
    spawn_knowledge_lifecycle_reconciler(shared.clone());
    restore_runtime_state(&shared).await;
    compact_system_events(&shared);
    spawn_outbox_worker(shared.clone());
    spawn_account_probe_refresh(shared.clone());
    crate::embed_runtime::spawn_embed_residue_sweeper(shared.clone());
    spawn_packet_self_heal_scanner(shared);
    Ok(())
}

async fn run_published_index_convergence_pass(shared: Arc<SharedState>) {
    let result = tokio::task::spawn_blocking(move || {
        crate::server::BlackboxServer::new(shared).converge_published_knowledge_at_startup()
    })
    .await;
    match result {
        Ok(report) if report.visited > 0 => tracing::info!(
            visited = report.visited,
            converged = report.converged,
            skipped = report.skipped,
            "published index reconciled from accepted content at startup"
        ),
        Ok(_) => {}
        Err(error) => tracing::warn!(
            %error,
            "startup published-index convergence pass failed to run"
        ),
    }
}

async fn run_knowledge_lifecycle_pass(shared: Arc<SharedState>) {
    let result = tokio::task::spawn_blocking(move || {
        let server = crate::server::BlackboxServer::new(shared);
        // Recovery is nonblocking even during startup. A live writer or
        // closeout retains its advisory lane; the periodic pass retries after
        // that owner releases it instead of delaying listener availability.
        let initial_reconciliation = server.reconcile_dark_knowledge_checkouts();
        let recovered = server.recover_abandoned_dark_knowledge_transactions();
        let reconciliation = if recovered > 0 {
            server.reconcile_dark_knowledge_checkouts()
        } else {
            initial_reconciliation
        };
        let inventory = server.run_knowledge_schema_epoch_inventory();
        let path_fallback = inventory
            .as_ref()
            .ok()
            .map(|report| server.reconcile_path_fallback_cut(report));
        (recovered, inventory, path_fallback, reconciliation)
    })
    .await;
    match result {
        Ok((recovered, inventory, path_fallback, reconciliation)) => {
            if recovered > 0 {
                tracing::info!(recovered, "knowledge transaction recovery completed");
            }
            match inventory {
                Ok(inventory) => tracing::info!(
                    resolved = inventory.inventory.resolved.len(),
                    quarantined = inventory.inventory.quarantined.len(),
                    marked_scopes = inventory.marked_scopes.len(),
                    "knowledge schema epoch inventoried"
                ),
                Err(err) => tracing::warn!(error = %err, "knowledge schema inventory failed"),
            }
            if let Some(path_fallback) = path_fallback {
                match path_fallback {
                    Ok(report) if report.newly_cut => {
                        tracing::info!("path-scoped project fallback retired")
                    }
                    Ok(report) if report.cut && !report.blockers.is_empty() => tracing::warn!(
                        blockers = ?report.blockers,
                        "post-cut path-scoped project debris remains quarantined from views"
                    ),
                    Ok(report) if !report.cut => tracing::debug!(
                        blockers = ?report.blockers,
                        "path-scoped project fallback remains enabled"
                    ),
                    Ok(_) => {}
                    Err(err) => tracing::warn!(
                        error = %err,
                        "path-scoped project fallback gate failed"
                    ),
                }
            }
            match reconciliation {
                Ok(reconciliation) => tracing::info!(
                    discovered = reconciliation.discovered,
                    dropped = reconciliation.dropped,
                    refreshed = reconciliation.refreshed,
                    "knowledge checkout lifecycle reconciled"
                ),
                Err(err) => {
                    tracing::warn!(error = %err, "knowledge checkout reconciliation failed")
                }
            }
        }
        Err(err) => tracing::warn!(error = %err, "knowledge lifecycle task failed"),
    }
}

fn spawn_knowledge_lifecycle_reconciler(shared: Arc<SharedState>) {
    let interval_secs = std::env::var("BBOX_KNOWLEDGE_RECONCILE_INTERVAL_SECS")
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(30);
    if interval_secs == 0 {
        tracing::debug!("knowledge checkout lifecycle reconciliation disabled");
        return;
    }
    tokio::spawn(async move {
        let interval = std::time::Duration::from_secs(interval_secs);
        loop {
            tokio::time::sleep(interval).await;
            run_knowledge_lifecycle_pass(shared.clone()).await;
        }
    });
}

/// Augment the daemon's process `PATH` once at startup so direct child-process
/// spawns (for example bro-harness and stdio MCP servers) resolve binaries the
/// launchd/systemd PATH omits. This is constant and one-time — NOT per-session —
/// so it does not reintroduce the serialize-everything lock that the §3
/// per-session work removed (shell tools augment PATH per-command themselves).
fn configure_dispatch_path_env() {
    let augmented = crate::orchestration::providers::dispatch_path_env();
    // SAFETY: one-time startup mutation before any session task is spawned.
    unsafe {
        std::env::set_var("PATH", augmented);
    }
}

fn configure_embed_runtime(shared: &Arc<SharedState>) {
    crate::embed_runtime::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    crate::embed_runtime::install_contradiction_state(shared.clone());
}

fn spawn_vector_warmup_thread(shared: Arc<SharedState>) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("blackbox-vectors-warmup".into())
        .spawn(move || {
            let started = std::time::Instant::now();
            let store = vectors::global();
            embed_queue::install(embed::queue::EmbedQueueHandle::start_default_with_store(
                store.clone(),
            ));
            // No provider-capable queue exists before durable vectors are
            // available. Wake convergence after activation so mutations that
            // landed during warmup are recovered without waiting for the
            // periodic sweep interval.
            crate::embed_runtime::queue_drain_wake("vector-store-warmed");
            tracing::info!(
                partitions = store.partition_count(),
                elapsed_ms = started.elapsed().as_millis(),
                "vector store warmed"
            );
            super::code_source::notify_cutback_readiness_available(&shared);
            super::file_source_activation::notify_connector_retirement_readiness_available(&shared);
        })
        .map_err(|e| anyhow::anyhow!("spawning vector store warmup thread: {e}"))?;
    Ok(())
}

fn spawn_storage_gc(shared: Arc<SharedState>) {
    let storage_gc_interval = storage_gc_interval_from_env();
    tracing::info!(
        interval_secs = storage_gc_interval.as_secs(),
        "storage GC maintenance thread enabled"
    );
    spawn_storage_gc_thread(shared, storage_gc_interval);
}

fn spawn_task_completed_router(shared: Arc<SharedState>) {
    let mut tail_rx = shared.tail_tx.subscribe();
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
                        shared.clone(),
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

fn spawn_system_event_signal_bridge(shared: Arc<SharedState>) {
    let mut system_event_rx = shared.system_events.subscribe();
    tokio::spawn(async move {
        loop {
            match system_event_rx.recv().await {
                Ok(event) => {
                    let signal = event.kind.to_wire().to_string();
                    let has_wait = shared
                        .wait_store
                        .snapshot()
                        .into_iter()
                        .any(|w| w.signal == signal);
                    if !has_wait {
                        continue;
                    }
                    let correlation = event.correlation.clone();
                    // Router-persisted idle signals deliver the caller's
                    // raw payload so templates see the same shape as a
                    // live delivery; other producers keep the envelope
                    // form consumers already depend on. Either way the
                    // event id travels for consumed-event bookkeeping.
                    let payload = if event.producer == "signal.router" {
                        event.payload.clone()
                    } else {
                        serde_json::to_value(&event).unwrap_or_else(|e| {
                            serde_json::json!({
                                "event_id": event.id,
                                "kind": signal,
                                "serialization_error": e.to_string(),
                            })
                        })
                    };
                    let resolved = signal_arc_dispatch(
                        &shared,
                        &signal,
                        correlation,
                        payload,
                        SignalDispatchOrigin::SystemEventBridge,
                        Some(event.id.clone()),
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

fn start_bbox_watcher(shared: &Arc<SharedState>) {
    let projects = shared.records_provider.records_snapshot().records;
    // Catalog mode registers by native attachment id, gated on the
    // `artifact_watching` capability (plan section 8, P5-F watcher items 1
    // and 2). Bridge mode keeps the Selected carrier over compatibility
    // records byte-identical.
    let project_carriers = match super::checkout_access::catalog_watch_carriers(shared) {
        super::checkout_access::CatalogWatchCarriers::Available(carriers) => carriers,
        super::checkout_access::CatalogWatchCarriers::Unavailable => {
            // Startup with unreadable catalog authority installs no native
            // registration rather than guessing one. The post-commit
            // reconciler converges the set once authority is readable.
            tracing::warn!(
                "catalog authority unavailable at watcher startup; native registrations deferred \
                 to post-commit reconciliation"
            );
            Vec::new()
        }
        super::checkout_access::CatalogWatchCarriers::BridgeMode => projects
            .iter()
            .filter_map(|project| {
                watcher::ArtifactWatchCarrier::selected(project.project_id.clone())
                    .map_err(|error| {
                        tracing::warn!(
                            project = %project.project_id,
                            error = %error,
                            "artifact watcher skipped invalid project carrier"
                        );
                    })
                    .ok()
            })
            .collect::<Vec<_>>(),
    };
    let watch_access = Arc::new(super::checkout_access::DaemonArtifactWatchAccess::new(
        shared.checkout_access.clone(),
        shared.project_authority.catalog_store().cloned(),
    ));
    let catalog = Arc::new(shared.artifacts.read().clone());

    // On a committed `.bbox/knowledge/` or top-level `.bbox/gaps/` change (e.g.
    // `git pull`, manual edit): reload the in-memory store(s) so
    // `bbox_knowledge`/`bbox_gaps`/`render`/`bbox_inbox` see it immediately, and
    // flag the reindex thread to refresh search on its next tick. A `Weak` ref
    // avoids a cycle — `SharedState` owns the watcher. The callback deliberately
    // does NOT touch the search index directly: the reindex thread is the single
    // tantivy writer, so this adds no writer contention and cannot deadlock
    // against `idx`/`kb` readers. Gaps are not search-indexed, so reloading them
    // is reload-only (no reindex contribution).
    let weak = Arc::downgrade(shared);
    let (refresh_tx, refresh_rx) = std::sync::mpsc::sync_channel::<()>(1);
    if let Err(error) = std::thread::Builder::new()
        .name("blackbox-knowledge-watch-refresh".into())
        .spawn(move || {
            while refresh_rx.recv().is_ok() {
                let Some(state) = weak.upgrade() else {
                    return;
                };
                {
                    let mut kb = state.kb.write();
                    if let Err(error) = kb.reload() {
                        tracing::warn!(
                            "watcher: kb reload after .bbox/knowledge change failed: {error:#}"
                        );
                    }
                }
                {
                    let mut gaps = state.gaps.write();
                    if let Err(error) = gaps.reload() {
                        tracing::warn!(
                            "watcher: gaps reload after .bbox/gaps change failed: {error:#}"
                        );
                    }
                }
                state
                    .reindex_dirty
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                let server = crate::server::BlackboxServer::new(state);
                if let Err(error) = server.reconcile_dark_knowledge_checkouts() {
                    tracing::warn!(
                        error = %error,
                        "watcher: checkout overlay reconciliation failed"
                    );
                }
                tracing::debug!(
                    "watcher: .bbox repo-store change reloaded, reconciled, and flagged reindex"
                );
            }
        })
    {
        tracing::warn!(error = %error, "watcher: could not start overlay refresh coordinator");
    }
    let on_knowledge_change: watcher::KnowledgeChangeCallback =
        Arc::new(move |_carrier| match refresh_tx.try_send(()) {
            Ok(()) | Err(std::sync::mpsc::TrySendError::Full(())) => {}
            Err(std::sync::mpsc::TrySendError::Disconnected(())) => {
                tracing::warn!("watcher: overlay refresh coordinator is unavailable");
            }
        });

    match watcher::BbxWatcher::start(
        project_carriers,
        watch_access,
        catalog,
        Some(on_knowledge_change),
    ) {
        Ok(mut w) => {
            for row in shared.checkout_registry.read().rows().to_vec() {
                let scope = match row.published_scope() {
                    Some(scope) => scope,
                    None => continue,
                };
                let project_id = if let Some(project_id) = row.project_id.clone() {
                    project_id
                } else {
                    match super::checkout_access::project_id_for_published_scope(
                        &shared.checkout_access,
                        projects.iter().map(|project| project.project_id.clone()),
                        &scope,
                    ) {
                        Ok(Some(project_id)) => project_id,
                        Ok(None) => {
                            tracing::warn!(
                                checkout_id = %row.checkout_id,
                                "provisional knowledge watcher has no registered project for scope"
                            );
                            continue;
                        }
                        Err(error) => {
                            tracing::warn!(
                                checkout_id = %row.checkout_id,
                                error = %error,
                                "provisional knowledge watcher could not resolve project scope"
                            );
                            continue;
                        }
                    }
                };
                if shared
                    .knowledge_transport_cutover
                    .covers_project_str(&project_id)
                {
                    continue;
                }
                let carrier = match watcher::ArtifactWatchCarrier::checkout(
                    project_id,
                    row.checkout_id.clone(),
                ) {
                    Ok(carrier) => carrier,
                    Err(error) => {
                        tracing::warn!(
                            checkout_id = %row.checkout_id,
                            error = %error,
                            "provisional knowledge watcher rejected checkout carrier"
                        );
                        continue;
                    }
                };
                if let Err(err) = w.watch_repo_store(carrier) {
                    tracing::warn!(
                        checkout_id = %row.checkout_id,
                        error = %err,
                        "provisional knowledge watcher failed to start"
                    );
                }
            }
            *shared.bbox_watcher.lock().unwrap() = Some(w);
            tracing::info!(".bbox/ watcher started (artifacts + knowledge + gaps)");
        }
        Err(e) => tracing::warn!(".bbox/ watcher failed to start: {e:#}"),
    }
}

fn compact_system_events(shared: &Arc<SharedState>) {
    let now = util::now_iso();
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

fn spawn_outbox_worker(shared: Arc<SharedState>) {
    tokio::spawn(async move {
        crate::system_events_runtime::worker::run_worker(shared).await;
    });
}

/// Periodically refresh provider account utilization probes (the producer the
/// allocator's `quota_capacity` consumer was always missing). Seeds immediately
/// at startup, then every `BBOX_ACCOUNT_PROBE_INTERVAL_SECS` (default 900;
/// 0 disables). v1 probes GLM/Z.AI; the prober suite extends to other providers.
fn spawn_account_probe_refresh(shared: Arc<SharedState>) {
    let interval_secs = std::env::var("BBOX_ACCOUNT_PROBE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(900);
    if interval_secs == 0 {
        tracing::debug!("account probe refresh: disabled (interval=0)");
        return;
    }
    let Some(home) = dirs::home_dir() else {
        tracing::warn!("account probe refresh: no home dir resolvable; disabled");
        return;
    };
    let store_dir = shared.store_dir.clone();
    tracing::info!(interval_secs, "account probe refresh: enabled");
    tokio::spawn(async move {
        // tokio interval fires its first tick immediately → seed at startup,
        // then refresh on cadence.
        let mut ticker = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
        loop {
            ticker.tick().await;
            let now = orchestration::now_ms();
            let written =
                orchestration::account_probes::refresh_account_probes(&store_dir, &home, now).await;
            if written > 0 {
                tracing::info!(probes = written, "account probe refresh: wrote probes");
            }
        }
    });
}

fn spawn_packet_self_heal_scanner(shared: Arc<SharedState>) {
    let scanner_config = ScannerConfig::from_env();
    if scanner_config.enabled {
        tracing::info!(
            interval_secs = scanner_config.interval.as_secs(),
            window_hours = scanner_config.window.as_secs() / 3600,
            no_match_threshold = scanner_config.no_match_threshold,
            fidelity_threshold = scanner_config.fidelity_threshold,
            "packet self-heal scanner: enabled"
        );
        tokio::spawn(async move {
            let cfg = scanner_config;
            let mut ticker = tokio::time::interval(cfg.interval);
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let result = {
                    let guard = shared.packets.read();
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
}

#[cfg(test)]
mod catalog_watcher_startup_tests {
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    use bbox_artifacts::watcher::ArtifactWatchAttachment;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
        ProjectId,
    };
    use bbox_indexing::project_catalog_store::CatalogCommittedEvent;

    use super::*;
    use crate::server::state::catalog_fixture::CatalogFixture;

    const PROJECT: &str = "proj_watch";
    const CAPABLE: &str = "att_00000000000000000000000000000e01";
    const INCAPABLE: &str = "att_00000000000000000000000000000e02";
    const CHECKOUT_ONE: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee01";
    const CHECKOUT_TWO: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeee02";

    /// A checkout the catalog authority will accept: real directory, real
    /// `.bbox` tree, and the checkout-id marker it reads back on every lease.
    fn checkout(root: &std::path::Path, name: &str, checkout_id: &str) -> std::path::PathBuf {
        let dir = root.join(name);
        std::fs::create_dir_all(dir.join(".bbox").join("local")).unwrap();
        std::fs::write(
            dir.join(".bbox").join("local").join("checkout-id"),
            format!("{checkout_id}\n"),
        )
        .unwrap();
        dir
    }

    fn attach(
        state: &Arc<SharedState>,
        attachment_id: &str,
        checkout_id: &str,
        dir: &std::path::Path,
        artifact_watching: bool,
    ) {
        let store = state
            .project_authority
            .catalog_store()
            .expect("catalog authority");
        let scope = CatalogFixture::scope(".");
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let attachment_id = AttachmentId::parse(attachment_id).unwrap();
        let dir = dir.to_string_lossy().into_owned();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_catalog, attachments| {
                attachments.attachments.insert(
                    attachment_id.clone(),
                    CheckoutAttachment {
                        attachment_id: attachment_id.clone(),
                        project_id: project_id.clone(),
                        checkout_id: checkout_id.to_string(),
                        checkout_dir: dir.clone(),
                        checkout_project_dir: dir.clone(),
                        project_root_relpath: ".".into(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: Some("refs/heads/main".into()),
                        capabilities: AttachmentCapabilities {
                            artifact_watching,
                            ..Default::default()
                        },
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-08-03T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    fn registered(state: &SharedState) -> Vec<bbox_artifacts::watcher::ArtifactWatchCarrier> {
        let guard = state.bbox_watcher.lock().unwrap();
        guard
            .as_ref()
            .map(|watcher| watcher.registered_carriers())
            .unwrap_or_default()
    }

    /// Poll until `predicate` holds or the deadline passes. The observer runs
    /// on its own thread with a poll interval, so a fixed sleep would either
    /// be flaky or slow; this is neither.
    fn wait_until(
        state: &SharedState,
        predicate: impl Fn(&[bbox_artifacts::watcher::ArtifactWatchCarrier]) -> bool,
    ) -> Vec<bbox_artifacts::watcher::ArtifactWatchCarrier> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let carriers = registered(state);
            if predicate(&carriers) {
                return carriers;
            }
            assert!(
                Instant::now() < deadline,
                "observer did not converge; last registrations: {carriers:#?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn catalog_state(fixture: &CatalogFixture) -> Arc<SharedState> {
        let server = fixture.server_with_checkout_authority();
        server.state.clone()
    }

    /// Startup registers by attachment id, and only for an attachment that
    /// records `artifact_watching`. The second attachment is the negative
    /// half: it is attached and healthy and still installs no watcher.
    #[test]
    fn startup_registers_capable_attachments_by_attachment_id() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let state = catalog_state(&fixture);
        attach(
            &state,
            CAPABLE,
            CHECKOUT_ONE,
            &checkout(&root, "capable", CHECKOUT_ONE),
            true,
        );
        attach(
            &state,
            INCAPABLE,
            CHECKOUT_TWO,
            &checkout(&root, "incapable", CHECKOUT_TWO),
            false,
        );

        start_bbox_watcher(&state);

        let carriers = registered(&state);
        assert_eq!(carriers.len(), 1, "{carriers:#?}");
        assert_eq!(
            carriers[0].attachment(),
            &ArtifactWatchAttachment::AttachmentId(CAPABLE.to_string())
        );
    }

    /// The post-commit observer path end to end: a real observer thread, a
    /// real watcher, and a real catalog. A duplicate delivery of the same
    /// committed event must leave the registration set exactly as it was.
    #[test]
    fn duplicate_observer_delivery_is_idempotent_and_detach_removes_the_watcher() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let state = catalog_state(&fixture);
        let capable = checkout(&root, "capable", CHECKOUT_ONE);

        // Start the watcher BEFORE the attachment exists, so every
        // registration below arrives through the observer rather than
        // through startup.
        start_bbox_watcher(&state);
        assert!(registered(&state).is_empty());

        let observer = state
            .project_authority
            .catalog_store()
            .expect("catalog authority")
            .commit_observer();
        super::super::code_source::spawn_commit_observer(&state);

        attach(&state, CAPABLE, CHECKOUT_ONE, &capable, true);
        let event = CatalogCommittedEvent {
            epoch: state
                .project_authority
                .catalog_store()
                .unwrap()
                .snapshot()
                .unwrap()
                .epoch(),
            changed_project_ids: std::collections::BTreeSet::from([PROJECT.to_string()]),
        };
        observer.push_for_test(event.clone());

        let after_first = wait_until(&state, |carriers| carriers.len() == 1);
        assert_eq!(
            after_first[0].attachment(),
            &ArtifactWatchAttachment::AttachmentId(CAPABLE.to_string())
        );

        // Duplicate delivery of the same committed event: the catalog has
        // not changed, so the registration set must be identical afterwards.
        observer.push_for_test(event.clone());
        observer.push_for_test(event);
        std::thread::sleep(Duration::from_secs(3));
        assert_eq!(
            registered(&state),
            after_first,
            "a duplicate committed event must not churn registrations"
        );

        // Detach through the same observer path: the registration goes, and
        // nothing is left to publish events for that attachment.
        CatalogFixture::detach_in_server(
            &crate::server::BlackboxServer::new(state.clone()),
            CAPABLE,
        );
        observer.push_for_test(CatalogCommittedEvent {
            epoch: state
                .project_authority
                .catalog_store()
                .unwrap()
                .snapshot()
                .unwrap()
                .epoch(),
            changed_project_ids: std::collections::BTreeSet::from([PROJECT.to_string()]),
        });
        wait_until(&state, |carriers| carriers.is_empty());

        state
            .reconciler_shutdown
            .read()
            .store(true, Ordering::Release);
    }

    /// Plan 5.2 fallback clause, end to end through the real observer loop:
    /// a commit delivered while catalog authority is UNREADABLE must not
    /// wedge the loop and must not tear down registrations; it schedules the
    /// bounded rescan instead, and the loop converges once authority
    /// returns.
    ///
    /// The store's own poison arm is what an unreadable pair looks like in
    /// production, so this exercises the real degradation rather than a mock.
    #[test]
    fn unreadable_authority_schedules_a_rescan_and_converges_when_it_returns() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let state = catalog_state(&fixture);
        let capable = checkout(&root, "capable", CHECKOUT_ONE);
        attach(&state, CAPABLE, CHECKOUT_ONE, &capable, true);

        start_bbox_watcher(&state);
        let registered_before = registered(&state);
        assert_eq!(
            registered_before.len(),
            1,
            "startup registered the capable attachment"
        );

        let store = state
            .project_authority
            .catalog_store()
            .expect("catalog authority")
            .clone();
        let observer = store.commit_observer();
        super::super::code_source::spawn_commit_observer(&state);

        // Authority goes unreadable, then a commit arrives.
        let restore = store
            .poison_for_test("checkpoint test: catalog pair unreadable")
            .expect("store was readable");
        observer.push_for_test(CatalogCommittedEvent {
            epoch: 1,
            changed_project_ids: std::collections::BTreeSet::from([PROJECT.to_string()]),
        });

        // The registration survives: an unreadable snapshot is not evidence
        // that nothing is capable, so nothing is torn down.
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if observer.pending_rescan_generation().is_some() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "observer never scheduled the bounded rescan"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            registered(&state),
            registered_before,
            "unreadable authority must not remove live registrations"
        );

        // Authority returns; the loop drains its own rescan and converges.
        store.unpoison_for_test(restore);
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if observer.pending_rescan_generation().is_none() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "observer never drained the rescan after authority returned"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(
            registered(&state),
            registered_before,
            "convergence restores the same registration set"
        );

        state
            .reconciler_shutdown
            .read()
            .store(true, Ordering::Release);
    }
}

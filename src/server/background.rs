use super::restore::restore_runtime_state;
use super::{SharedState, dispatch_routed_event, spawn_edge_index_rebuild_watcher};
use crate::packets::ScannerConfig;
use crate::server::routes::signal_arc_dispatch;
use crate::server::storage_gc::{spawn_storage_gc_thread, storage_gc_interval_from_env};
use crate::tools::badgey_adapter::{
    BadgeyAgentAdapter, recover_badgey_non_terminal_state, restore_badgey_registry_from_notes,
};
use crate::tools::bro_helpers::tier0_cosine_threshold_from_env;
use crate::{embed, embed_queue, orchestration, system_events, util, vectors, watcher};
use std::sync::Arc;

pub(super) async fn start_background_tasks(shared: Arc<SharedState>) -> anyhow::Result<()> {
    install_badgey_adapter(&shared);
    restore_badgey_registry_from_notes(&shared);
    recover_badgey_non_terminal_state(&shared);
    configure_embed_queue(&shared);
    spawn_vector_warmup_thread()?;
    spawn_edge_index_rebuild_watcher(shared.clone(), std::time::Duration::from_secs(60));
    spawn_storage_gc(shared.clone());
    spawn_task_completed_router(shared.clone());
    spawn_system_event_signal_bridge(shared.clone());
    start_bbox_watcher(&shared);
    restore_runtime_state(&shared).await;
    compact_system_events(&shared);
    spawn_outbox_worker(shared.clone());
    spawn_account_probe_refresh(shared.clone());
    spawn_packet_self_heal_scanner(shared);
    Ok(())
}

fn install_badgey_adapter(shared: &Arc<SharedState>) {
    shared
        .agent_adapter_registry
        .write()
        .register(Arc::new(BadgeyAgentAdapter {
            state: shared.clone(),
        }));
}

fn configure_embed_queue(shared: &Arc<SharedState>) {
    embed_queue::install_contradiction_threshold(tier0_cosine_threshold_from_env());
    embed_queue::install_contradiction_state(shared.clone());
    embed_queue::install(embed::queue::EmbedQueueHandle::start_default_without_store());
}

fn spawn_vector_warmup_thread() -> anyhow::Result<()> {
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
                    let payload = serde_json::to_value(&event).unwrap_or_else(|e| {
                        serde_json::json!({
                            "event_id": event.id,
                            "kind": signal,
                            "serialization_error": e.to_string(),
                        })
                    });
                    let resolved =
                        signal_arc_dispatch(&shared, &signal, correlation, payload).await;
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
    let project_roots: Vec<(String, std::path::PathBuf)> = shared
        .projects
        .read()
        .list()
        .into_iter()
        .map(|r| (r.project_id, std::path::PathBuf::from(r.canonical_path)))
        .collect();
    let catalog = Arc::new(shared.artifacts.read().clone());

    // On a committed `.bbox/knowledge/` change (e.g. `git pull`, manual edit):
    // reload the in-memory knowledge store so `bbox_knowledge`/`render` see it
    // immediately, and flag the reindex thread to refresh search on its next
    // tick. A `Weak` ref avoids a cycle — `SharedState` owns the watcher. The
    // callback deliberately does NOT touch the search index directly: the
    // reindex thread is the single tantivy writer, so this adds no writer
    // contention and cannot deadlock against `idx`/`kb` readers.
    let weak = Arc::downgrade(shared);
    let on_knowledge_change: watcher::KnowledgeChangeCallback = Arc::new(move || {
        let Some(state) = weak.upgrade() else {
            return;
        };
        {
            let mut kb = state.kb.write();
            if let Err(e) = kb.reload() {
                tracing::warn!("watcher: kb reload after .bbox/knowledge change failed: {e:#}");
            }
        }
        state
            .reindex_dirty
            .store(true, std::sync::atomic::Ordering::Relaxed);
        tracing::debug!("watcher: .bbox/knowledge change → kb reloaded, reindex flagged");
    });

    match watcher::BbxWatcher::start(project_roots, catalog, Some(on_knowledge_change)) {
        Ok(w) => {
            *shared.bbox_watcher.lock().unwrap() = Some(w);
            tracing::info!(".bbox/ watcher started (artifacts + knowledge)");
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
        system_events::worker::run_worker(shared).await;
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

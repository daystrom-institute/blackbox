use super::{SharedState, routes};
use crate::{crons, pollers, webhooks, workflow};
use std::ffi::OsStr;
use std::sync::Arc;

pub(super) async fn restore_runtime_state(shared: &Arc<SharedState>) {
    restore_webhooks(shared);
    restore_pollers(shared);
    restore_crons(shared);
    restore_whiteboards(shared);
    restore_workflows(shared);
    restore_catalog_runtime_artifacts(shared);
    restore_reactions(shared).await;
    recover_outbox(shared);
}

fn restore_webhooks(shared: &Arc<SharedState>) {
    // Re-run install_check at restore time: a webhook installed under loopback
    // must not silently re-enable after the daemon is rebound publicly.
    let webhook_dir = shared.store_dir.join("webhooks");
    for spec in webhooks::load_all(&webhook_dir) {
        match webhooks::install_check(&spec.signature, shared.bind_is_loopback) {
            Ok(()) => {
                tracing::info!("restoring webhook '{}'", spec.name);
                shared.webhooks.install(spec);
            }
            Err(e) => {
                tracing::warn!(
                    "skipping restore of webhook '{}': install_check failed: {e}",
                    spec.name
                );
            }
        }
    }
}

fn restore_pollers(shared: &Arc<SharedState>) {
    let poller_dir = shared.store_dir.join("pollers");
    for spec in pollers::load_all(&poller_dir) {
        tracing::info!(
            "restoring poller '{}' (every {}s)",
            spec.name,
            spec.every_seconds
        );
        shared.pollers.install(spec.clone());
        let handle = pollers::spawn_loop(shared.clone(), spec.clone());
        shared.pollers.track_handle(&spec.name, handle);
    }
}

fn restore_crons(shared: &Arc<SharedState>) {
    let cron_dir = shared.store_dir.join("crons");
    for spec in crons::load_all(&cron_dir) {
        match crons::validate_schedule(&spec.schedule) {
            Ok(()) => {
                tracing::info!(
                    "restoring cron '{}' (schedule '{}', concurrency {})",
                    spec.name,
                    spec.schedule,
                    spec.concurrency
                );
                shared.crons.install(spec.clone());
                let handle = crons::spawn_loop(shared.clone(), spec.clone());
                shared.crons.track_handle(&spec.name, handle);
            }
            Err(e) => {
                tracing::warn!("skipping restore of cron '{}': {e}", spec.name);
            }
        }
    }
}

fn restore_whiteboards(shared: &Arc<SharedState>) {
    let whiteboard_dir = shared.store_dir.join("whiteboards");
    if let Err(e) = shared.whiteboards.set_storage_dir(whiteboard_dir) {
        tracing::warn!("whiteboards storage init failed: {e}");
    } else {
        let restored = shared.whiteboards.list_ids().len();
        if restored > 0 {
            tracing::info!("restored {restored} active whiteboard(s)");
        }
    }
}

fn restore_workflows(shared: &Arc<SharedState>) {
    let workflow_dir = shared.store_dir.join("workflows");
    if !workflow_dir.exists() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&workflow_dir) else {
        return;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension() != Some(OsStr::new("json")) {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(spec) = serde_json::from_slice::<workflow::Workflow>(&bytes) {
                let id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or(&spec.name)
                    .to_string();
                tracing::info!("restoring workflow '{id}'");
                shared.workflow_registry.write().insert(id, spec);
            }
        }
    }
}

fn restore_catalog_runtime_artifacts(shared: &Arc<SharedState>) {
    match routes::restore_runtime_artifacts_from_catalog(shared) {
        Ok(restored) if restored > 0 => {
            tracing::info!(
                "restored {restored} workflow/packet/brofile runtime artifact(s) from active catalog"
            );
        }
        Ok(_) => {}
        Err(err) => {
            tracing::warn!(
                "failed to restore workflow/packet/brofile runtime artifacts from active catalog: {err:#}"
            );
        }
    }
}

async fn restore_reactions(shared: &Arc<SharedState>) {
    let reaction_warnings = shared.system_events.restore_reactions_from_disk().await;
    if !reaction_warnings.is_empty() {
        tracing::warn!("reaction restore: {} warning(s)", reaction_warnings.len());
    }
}

fn recover_outbox(shared: &Arc<SharedState>) {
    let recovery = shared.system_events.outbox_store().recover_stale_claims();
    if recovery.requeued > 0 || recovery.dead_lettered > 0 {
        tracing::info!(
            "outbox recovery: {} requeued, {} dead-lettered",
            recovery.requeued,
            recovery.dead_lettered
        );
    }
}

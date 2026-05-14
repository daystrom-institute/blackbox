use super::*;
use crate::storage_health;
use std::sync::Arc;

const DEFAULT_STORAGE_GC_INTERVAL_SECS: u64 = 6 * 60 * 60;

pub(crate) fn storage_gc_interval_from_env() -> std::time::Duration {
    let secs = std::env::var("BLACKBOX_STORAGE_GC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_STORAGE_GC_INTERVAL_SECS);
    std::time::Duration::from_secs(secs)
}

pub(crate) fn spawn_storage_gc_thread(state: Arc<SharedState>, interval: std::time::Duration) {
    let _ = std::thread::Builder::new()
        .name("blackbox-storage-gc".into())
        .spawn(move || {
            // Avoid competing with startup rebuilds, watcher initialization, and
            // client handshakes. Operators can still run bbox_storage_gc manually.
            std::thread::sleep(interval);
            loop {
                if let Err(err) = run_storage_gc_pass(&state) {
                    tracing::warn!(error = %err, "storage GC maintenance pass failed");
                }
                std::thread::sleep(interval);
            }
        })
        .map_err(|err| tracing::warn!(error = %err, "failed to spawn storage GC thread"));
}

fn run_storage_gc_pass(state: &SharedState) -> anyhow::Result<()> {
    let edges_dir = storage_health::find_edges_dir(&state.store_dir, None);
    let registered: std::collections::HashSet<String> = state
        .projects
        .read()
        .list()
        .into_iter()
        .map(|project| project.project_id)
        .collect();
    let params = storage_health::GcParams {
        dry_run: false,
        project_filter: None,
        prune_backups: true,
        prune_orphans: true,
        prune_temps: true,
        prune_inactive_snapshots: true,
        max_backup_age_days: Some(7),
        keep_newest_backup_per_source: 1,
    };
    let candidates =
        storage_health::plan_gc_with_policy(&edges_dir, &registered, &params, &Default::default())?;
    let deletable_bytes: u64 = candidates
        .iter()
        .filter(|candidate| candidate.deletable)
        .map(|candidate| candidate.bytes)
        .sum();
    let deletable_count = candidates
        .iter()
        .filter(|candidate| candidate.deletable)
        .count();
    if deletable_count == 0 {
        tracing::debug!("storage GC maintenance pass found no deletable candidates");
        return Ok(());
    }

    let (deleted, errors) = storage_health::apply_gc(&candidates);
    if errors.is_empty() {
        tracing::info!(
            deleted = deleted.len(),
            planned = deletable_count,
            bytes = deletable_bytes,
            "storage GC maintenance pass applied"
        );
    } else {
        tracing::warn!(
            deleted = deleted.len(),
            planned = deletable_count,
            bytes = deletable_bytes,
            errors = ?errors,
            "storage GC maintenance pass partially applied"
        );
    }
    Ok(())
}

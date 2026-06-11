use std::sync::Arc;

use crate::server::SharedState;
use crate::storage_health;

const DEFAULT_STORAGE_GC_INTERVAL_SECS: u64 = 6 * 60 * 60;

/// Age floor for the maintenance pass's inactive-snapshot pruning. The
/// interactive `bbox_storage_gc` default (14d) let per-commit snapshot churn
/// accumulate ~24GB in one heavy week (145 generations, one per HEAD commit)
/// before anything aged out. Snapshot reuse value lives in the keep-recent
/// retention (3/workspace + 10/repo), which still applies regardless of this
/// floor; 2 days of extra age protection covers branch-switch round-trips.
const DEFAULT_SNAPSHOT_MAX_AGE_DAYS: u64 = 2;

pub(crate) fn storage_gc_interval_from_env() -> std::time::Duration {
    let secs = std::env::var("BLACKBOX_STORAGE_GC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT_STORAGE_GC_INTERVAL_SECS);
    std::time::Duration::from_secs(secs)
}

/// `BLACKBOX_STORAGE_GC_SNAPSHOT_MAX_AGE_DAYS` overrides the maintenance
/// pass's snapshot age floor. `0` disables age-based retention entirely
/// (keep-recent rules still protect the newest snapshots).
fn snapshot_max_age_days_from_env() -> u64 {
    std::env::var("BLACKBOX_STORAGE_GC_SNAPSHOT_MAX_AGE_DAYS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_SNAPSHOT_MAX_AGE_DAYS)
}

pub(crate) fn spawn_storage_gc_thread(state: Arc<SharedState>, interval: std::time::Duration) {
    let _ = std::thread::Builder::new()
        .name("blackbox-storage-gc".into())
        .spawn(move || {
            let _scope = crate::util::BlockingScope::enter();
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
    let mut policy = storage_health::GcPolicy::default();
    policy.materialized_snapshots.max_age_days = Some(snapshot_max_age_days_from_env());
    let candidates =
        storage_health::plan_gc_with_policy(&edges_dir, &registered, &params, &policy)?;
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

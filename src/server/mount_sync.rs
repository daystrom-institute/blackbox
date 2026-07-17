//! Periodic per-mount resync loop — background-maintenance-loop precedent
//! shared with `server::storage_gc` (sync, OS-thread) and
//! `embed_runtime::spawn_embed_residue_sweeper` (async, tokio-native this
//! loop follows since mount sync is genuinely async). Per-mount errors are
//! recorded on the `MountRecord`'s `last_sync` (by `connectors_runtime`) and
//! logged here; never fatal to the loop. Mounts already mid-sync (a manual
//! `bbox_mount_sync`, or a prior tick that overran the interval) are skipped
//! via the same in-flight guard `connectors_runtime::sync_one_mount` uses.

use std::sync::Arc;
use std::time::Duration;

use crate::connectors_runtime;
use crate::server::state::SharedState;

const DEFAULT_MOUNT_SYNC_INTERVAL_SECS: u64 = 900;

pub(crate) fn mount_sync_interval_from_env() -> Duration {
    let secs = std::env::var("BLACKBOX_MOUNT_SYNC_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MOUNT_SYNC_INTERVAL_SECS);
    Duration::from_secs(secs)
}

/// Spawn the periodic mount resync loop. `0` (via
/// `BLACKBOX_MOUNT_SYNC_INTERVAL_SECS`) disables it entirely — operators can
/// still drive syncs manually with `bbox_mount_sync`.
pub(crate) fn spawn_mount_sync_loop(state: Arc<SharedState>) {
    let interval = mount_sync_interval_from_env();
    if interval.is_zero() {
        tracing::info!("mount sync loop: disabled (BLACKBOX_MOUNT_SYNC_INTERVAL_SECS=0)");
        return;
    }
    tracing::info!(
        interval_secs = interval.as_secs(),
        "mount sync loop: enabled"
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            run_pass(&state).await;
        }
    });
}

async fn run_pass(state: &Arc<SharedState>) {
    let mount_ids: Vec<String> = state
        .mounts
        .read()
        .list()
        .into_iter()
        .map(|record| record.mount_id)
        .collect();
    for mount_id in mount_ids {
        // full=false: an ordinary periodic pass resumes from the stored
        // cursor. project_alias=None: registration (if this happens to be a
        // mount's first successful sync, e.g. a retried initial sync) needs
        // no alias here — the operator's bbox_mount_register call already
        // carried one, if any, and that pass already ran or will run its
        // own registration attempt.
        match connectors_runtime::run_sync_and_register_project(state, &mount_id, false, None).await
        {
            Ok(summary) => {
                if !summary.errors.is_empty() {
                    tracing::warn!(
                        mount_id = %mount_id,
                        errors = ?summary.errors,
                        "periodic mount sync completed with per-entry errors"
                    );
                }
                if !summary.degradations.is_empty() {
                    tracing::warn!(
                        mount_id = %mount_id,
                        degradations = ?summary.degradations,
                        "periodic mount sync degraded"
                    );
                }
            }
            Err(err) => {
                if err
                    .downcast_ref::<connectors_runtime::MountBusy>()
                    .is_some()
                {
                    tracing::debug!(mount_id = %mount_id, "periodic mount sync skipped: already in flight");
                } else {
                    tracing::warn!(mount_id = %mount_id, error = format_args!("{err:#}"), "periodic mount sync failed");
                }
            }
        }
    }
}

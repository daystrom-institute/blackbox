use super::{SharedState, routes};
use std::sync::Arc;

pub(super) async fn restore_runtime_state(shared: &Arc<SharedState>) {
    restore_whiteboards(shared);
    // Retired schedules, workflow checkpoints and reaction outboxes remain
    // inert on disk. Startup does not claim, rewrite or replay their records.
    if let Err(error) = routes::restore_runtime_artifacts_from_catalog(shared) {
        tracing::warn!(%error, "failed to restore retained runtime artifacts");
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

use super::{SharedState, routes};
use std::sync::Arc;

pub(super) async fn restore_runtime_state(shared: &Arc<SharedState>) {
    // Retired schedules, workflow checkpoints and reaction outboxes remain
    // inert on disk. Startup does not claim, rewrite or replay their records.
    if let Err(error) = routes::restore_runtime_artifacts_from_catalog(shared) {
        tracing::warn!(%error, "failed to restore retained runtime artifacts");
    }
}

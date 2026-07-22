//! Daemon factory for the version-1 checkout compatibility authority.

use std::sync::Arc;

use bbox_indexing::checkout_access::CheckoutAccessBroker;

use super::SharedState;

/// Return the daemon-owned broker over the shared version-1 registries and
/// observation store. Reindex and staging must use this same handle so their
/// authority state and counters cannot diverge.
#[allow(dead_code)] // Consumer migration follows the Phase 0 primitive and factory.
pub(crate) fn checkout_access_broker(state: &Arc<SharedState>) -> Arc<CheckoutAccessBroker> {
    state.checkout_access.clone()
}

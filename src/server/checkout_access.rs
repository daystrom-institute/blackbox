//! Daemon factory for the version-1 checkout compatibility authority.

use std::sync::Arc;

use bbox_indexing::checkout_access::CheckoutAccessBroker;
use bbox_indexing::checkout_access_v1::V1CheckoutAccessAuthority;

use super::SharedState;

/// Build a broker over the shared version-1 registries and observation store.
/// The authority itself lives in `bbox-indexing` so reindex and staging paths
/// can consume the same boundary without depending on daemon state.
#[allow(dead_code)] // Consumer migration follows the Phase 0 primitive and factory.
pub(crate) fn checkout_access_broker(state: &Arc<SharedState>) -> CheckoutAccessBroker {
    let authority =
        V1CheckoutAccessAuthority::new(state.projects.clone(), state.checkout_registry.clone());
    CheckoutAccessBroker::new(
        Arc::new(authority),
        state.checkout_access_observations.clone(),
    )
}

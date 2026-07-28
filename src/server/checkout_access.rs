//! Daemon adapters for the version-1 checkout compatibility authority.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use bbox_artifacts::watcher::{
    ArtifactWatchAccess, ArtifactWatchAttachment, ArtifactWatchCarrier, ArtifactWatchRead,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector, ValidatedCheckoutLease,
};

use super::SharedState;

/// Return the daemon-owned broker over the shared version-1 registries and
/// observation store. Reindex and staging must use this same handle so their
/// authority state and counters cannot diverge.
#[allow(dead_code)] // Consumer migration follows the Phase 0 primitive and factory.
pub(crate) fn checkout_access_broker(state: &Arc<SharedState>) -> Arc<CheckoutAccessBroker> {
    state.checkout_access.clone()
}

/// Resolve one published scope back to its unique registered project using
/// brokered config-tree discovery. No caller path participates in the lookup.
pub(crate) fn project_id_for_published_scope(
    broker: &CheckoutAccessBroker,
    project_ids: impl IntoIterator<Item = String>,
    expected_scope: &PublishedScope,
) -> Result<Option<String>> {
    let mut matched = None;
    for project_id in project_ids {
        // One unhealthy project must not poison the whole scope lookup
        // (review): a project whose checkout is gone, detached, or lacking
        // the discovery capability is simply not the publisher; continue.
        let lease = match broker.acquire(CheckoutAccessRequest {
            project_id: project_id.clone(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        }) {
            Ok(lease) => lease,
            Err(error)
                if matches!(
                    error.code,
                    bbox_indexing::checkout_access::CheckoutAccessErrorCode::AttachmentNotFound
                        | bbox_indexing::checkout_access::CheckoutAccessErrorCode::AttachmentInactive
                        | bbox_indexing::checkout_access::CheckoutAccessErrorCode::CapabilityDenied
                ) =>
            {
                tracing::debug!(
                    project_id = %project_id,
                    error_code = %error.code.as_str(),
                    "scope discovery skipped an unavailable project"
                );
                continue;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error)).with_context(|| {
                    format!("discovering published scope for project {project_id}")
                });
            }
        };
        let is_match = lease.published_scope() == Some(expected_scope);
        broker.revalidate(&lease).map_err(anyhow::Error::new)?;
        if !is_match {
            continue;
        }
        if matched.replace(project_id).is_some() {
            anyhow::bail!("published scope resolves to more than one registered project");
        }
    }
    Ok(matched)
}

/// Read one registered project's committed published scope exclusively through
/// checkout authority. `None` is the explicit legacy-local state.
pub(crate) fn published_scope_for_project(
    broker: &CheckoutAccessBroker,
    project_id: &str,
) -> Result<Option<PublishedScope>> {
    with_selected_project_access(
        broker,
        project_id,
        CheckoutAccessKind::PublisherConfigTreeRead,
        CheckoutAccessIntent::Read,
        |lease| Ok(lease.published_scope().cloned()),
    )
}

/// Run one selected-attachment operation under the requested capability. The
/// v1 bridge first acquires the sole scope-discovery lease needed to bind the
/// legacy path record to its current published scope.
pub(crate) fn with_selected_project_access<T>(
    broker: &CheckoutAccessBroker,
    project_id: &str,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
    operation: impl FnOnce(&ValidatedCheckoutLease) -> Result<T>,
) -> Result<T> {
    let lease = acquire_selected_project_access(broker, project_id, kind, intent)?;
    let outcome = operation(&lease);
    broker.revalidate(&lease).map_err(anyhow::Error::new)?;
    outcome
}

/// Acquire one selected-attachment lease for a caller that must retain more
/// than one capability until a single publication boundary.
pub(crate) fn acquire_selected_project_access(
    broker: &CheckoutAccessBroker,
    project_id: &str,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<ValidatedCheckoutLease> {
    let scope_lease = broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_owned(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(anyhow::Error::new)?;
    if kind == CheckoutAccessKind::PublisherConfigTreeRead && intent == CheckoutAccessIntent::Read {
        return Ok(scope_lease);
    }
    let expected_scope = scope_lease.published_scope().cloned();
    drop(scope_lease);
    broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_owned(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope,
            kind,
            intent,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(anyhow::Error::new)
}

/// Run an exact checkout operation from the registry snapshot already held by
/// `parent`. The resulting lease shares the same lifecycle guard.
/// Run one exact registered checkout operation without consulting either raw
/// path carried by the compatibility checkout descriptor.
pub(crate) fn with_resolved_checkout_access<T>(
    broker: &CheckoutAccessBroker,
    checkout: &ResolvedCheckoutScope,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
    operation: impl FnOnce(&ValidatedCheckoutLease) -> Result<T>,
) -> Result<T> {
    let lease = broker
        .acquire(CheckoutAccessRequest {
            project_id: checkout.project_id.clone(),
            attachment: CheckoutAttachmentSelector::CheckoutId(checkout.checkout_id.clone()),
            expected_scope: Some(checkout.published_scope.clone()),
            kind,
            intent,
            source_lane: CheckoutAccessSourceLane::LegacyCheckoutRegistry,
        })
        .map_err(anyhow::Error::new)?;
    let outcome = operation(&lease);
    broker.revalidate(&lease).map_err(anyhow::Error::new)?;
    outcome
}

/// Watcher-facing adapter. Registrations and every event batch reacquire an
/// `ArtifactWatchDiscovery` lease from their path-free logical carrier.
pub(crate) struct DaemonArtifactWatchAccess {
    broker: Arc<CheckoutAccessBroker>,
}

impl DaemonArtifactWatchAccess {
    pub(crate) fn new(broker: Arc<CheckoutAccessBroker>) -> Self {
        Self { broker }
    }
}

impl ArtifactWatchAccess for DaemonArtifactWatchAccess {
    fn with_discovery(
        &self,
        carrier: &ArtifactWatchCarrier,
        prepare: &mut dyn FnMut(&dyn ArtifactWatchRead) -> Result<()>,
        publish: &mut dyn FnMut(&dyn ArtifactWatchRead) -> Result<()>,
    ) -> Result<()> {
        let scope_lease = self
            .broker
            .acquire(CheckoutAccessRequest {
                project_id: carrier.project_id().to_owned(),
                attachment: CheckoutAttachmentSelector::Selected,
                expected_scope: None,
                kind: CheckoutAccessKind::PublisherConfigTreeRead,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
            })
            .map_err(anyhow::Error::new)?;
        let expected_scope = scope_lease.published_scope().cloned();
        drop(scope_lease);
        let (attachment, source_lane) = match carrier.attachment() {
            ArtifactWatchAttachment::Selected => (
                CheckoutAttachmentSelector::Selected,
                CheckoutAccessSourceLane::LegacyProjectRecord,
            ),
            ArtifactWatchAttachment::CheckoutId(checkout_id) => (
                CheckoutAttachmentSelector::CheckoutId(checkout_id.clone()),
                CheckoutAccessSourceLane::LegacyCheckoutRegistry,
            ),
        };
        let lease = self
            .broker
            .acquire(CheckoutAccessRequest {
                project_id: carrier.project_id().to_owned(),
                attachment,
                expected_scope,
                kind: CheckoutAccessKind::ArtifactWatchDiscovery,
                intent: CheckoutAccessIntent::Read,
                source_lane,
            })
            .map_err(anyhow::Error::new)?;
        prepare(&LeaseArtifactWatchRead { lease: &lease })?;
        let _publication = self
            .broker
            .publication_guard(&lease)
            .map_err(anyhow::Error::new)?;
        publish(&LeaseArtifactWatchRead { lease: &lease })
    }
}

struct LeaseArtifactWatchRead<'a> {
    lease: &'a ValidatedCheckoutLease,
}

impl ArtifactWatchRead for LeaseArtifactWatchRead<'_> {
    fn project_root(&self) -> &Path {
        self.lease.project_root()
    }

    fn read_relative_file(&self, relative: &Path) -> Result<Vec<u8>> {
        self.lease
            .read_relative_file(relative)
            .map(|(_, bytes)| bytes)
            .map_err(anyhow::Error::new)
    }

    fn check_relative_absence(&self, relative: &Path) -> Result<bool> {
        Ok(!self.lease.relative_regular_file_exists(relative)?)
    }
}

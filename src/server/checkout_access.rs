//! Daemon adapters for the version-1 checkout compatibility authority.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use bbox_artifacts::watcher::{
    ArtifactWatchAccess, ArtifactWatchAttachment, ArtifactWatchCarrier, ArtifactWatchRead,
};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentStatus, ProjectId, ProjectScope,
};
use bbox_corpus_core::project_record::ResolvedCheckoutScope;
use bbox_corpus_core::project_selector::{
    ProjectSelectorRequest, ResolveIntent, ResolvedAttachment, ResolvedProjectIdentity,
    SelectorClass, SessionCheckoutRef,
};
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessError, CheckoutAccessIntent, CheckoutAccessKind,
    CheckoutAccessRequest, CheckoutAccessSourceLane, CheckoutAttachmentSelector,
    ValidatedCheckoutLease,
};

use super::SharedState;
use crate::server::BlackboxServer;

/// The catalog lease refusals adapters surface, in the code-prefixed shape
/// the tool boundary renders through `err_text` (plan section 4.18).
fn checkout_access_error(error: CheckoutAccessError) -> anyhow::Error {
    anyhow!(
        "error.checkout_access.{}: {}",
        error.code.as_str(),
        error.diagnostic
    )
}

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

/// One selected attachment plus the scope its project already records.
///
/// Catalog scope comes from the catalog project row, so no preliminary
/// `PublisherConfigTreeRead` lease is needed to discover it. That matters
/// beyond cost: `PublisherConfigTreeRead` rides `repo_knowledge` (D-032), so
/// discovering scope through it would gate blame, render, and provenance on
/// a capability the section 9 table does not assign them.
pub(crate) struct CatalogAttachmentTarget {
    pub(crate) attachment_id: String,
    pub(crate) expected_scope: Option<PublishedScope>,
}

fn catalog_project_scope(project: &ResolvedProjectIdentity) -> Option<PublishedScope> {
    match project {
        ResolvedProjectIdentity::Catalog { project } => match &project.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal => None,
        },
        ResolvedProjectIdentity::V1Compat { .. } => None,
    }
}

/// The unique active `Base` attachment, or `None` when the project has none
/// or more than one.
fn unique_active_base_attachment(
    server: &BlackboxServer,
    project_id: &str,
) -> Option<CatalogAttachmentTarget> {
    let store = server.state.project_authority.catalog_store()?;
    let state = store.snapshot().ok()?;
    let parsed = ProjectId::parse(project_id).ok()?;
    let project = state.catalog().projects.get(&parsed)?;
    let mut bases = state.attachments().attachments.values().filter(|row| {
        row.status == AttachmentStatus::Attached
            && row.project_id == parsed
            && row.kind == AttachmentKind::Base
    });
    let base = bases.next()?;
    if bases.next().is_some() {
        return None;
    }
    Some(CatalogAttachmentTarget {
        attachment_id: base.attachment_id.as_str().to_string(),
        expected_scope: match &project.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal => None,
        },
    })
}

/// Select one catalog attachment for an operation named by project identity.
///
/// The shared resolver owns the ladder: explicit attachment, session
/// checkout, operator-selected default, single active attachment. D-033
/// item 3 fixes the unique active `Base` attachment as the final rung and
/// the resolver does not implement it, so it is applied here and ONLY where
/// the resolver reported ambiguity. Applying it earlier would redirect a
/// project whose default or sole attachment already resolved.
pub(crate) fn catalog_attachment_target(
    server: &BlackboxServer,
    project_id: &str,
) -> Result<CatalogAttachmentTarget> {
    let session = server.authoritative_session_checkout();
    let request = ProjectSelectorRequest {
        selector: Some(project_id.to_owned()),
        session: session.as_deref().map(|checkout| SessionCheckoutRef {
            checkout_id: Some(checkout.checkout_id.clone()),
            checkout_project_dir: None,
        }),
        intent: ResolveIntent::Read,
        class: SelectorClass::Selection,
        ..Default::default()
    };
    let resolved = server.with_project_resolver(|engine| engine.resolve_attached(&request))?;
    let resolved = match resolved {
        Ok(resolved) => resolved,
        Err(error) if error.code() == "error.project_attachment_ambiguous" => {
            return unique_active_base_attachment(server, project_id)
                .ok_or_else(|| anyhow::Error::new(error));
        }
        Err(error) => return Err(anyhow::Error::new(error)),
    };
    let ResolvedAttachment::Catalog { attachment_id, .. } = &resolved.attachment else {
        bail!("error.project_attachment_required: {project_id}");
    };
    Ok(CatalogAttachmentTarget {
        attachment_id: attachment_id.clone(),
        expected_scope: catalog_project_scope(&resolved.project),
    })
}

/// Acquire one lease by catalog attachment for an operation named by project
/// identity alone.
///
/// CATALOG MODE ONLY, and deliberately not a variant of
/// [`acquire_selected_project_access`]. The bridge helper above must first
/// take a `PublisherConfigTreeRead` lease purely to discover the published
/// scope, because a version-1 record carries none. Reusing that shape here
/// would be a capability defect, not just wasted work: `PublisherConfigTreeRead`
/// rides `repo_knowledge` (D-032), so every catalog blame, render, provenance
/// and file read would be gated on a capability the Phase 5 section 9 table
/// does not assign it, and a render-capable attachment lacking `repo_knowledge`
/// would be denied for the wrong reason. Each adapter must gate on its own
/// capability bit alone. The catalog attachment already carries
/// `validated_scope`, so the preliminary lease buys nothing anyway.
///
/// The bridge helpers keep their two-step and their refusal strings verbatim;
/// other bridge callers depend on both.
pub(crate) fn acquire_catalog_project_lease(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    project_id: &str,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<ValidatedCheckoutLease> {
    let target = catalog_attachment_target(server, project_id)?;
    broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_string(),
            attachment: CheckoutAttachmentSelector::AttachmentId(target.attachment_id),
            expected_scope: target.expected_scope,
            kind,
            intent,
            source_lane: CheckoutAccessSourceLane::NativeAttachment,
        })
        .map_err(checkout_access_error)
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

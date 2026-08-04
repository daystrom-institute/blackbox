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
use bbox_indexing::project_catalog_store::ProjectCatalogStore;

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
    /// Present in catalog mode only. Used solely to read a project's
    /// recorded scope for the lease's `expected_scope`; the attachment id in
    /// the carrier remains the authority for which checkout is opened.
    catalog: Option<Arc<ProjectCatalogStore>>,
}

impl DaemonArtifactWatchAccess {
    pub(crate) fn new(broker: Arc<CheckoutAccessBroker>) -> Self {
        Self {
            broker,
            catalog: None,
        }
    }

    /// The catalog-mode adapter. Native attachment carriers resolve their
    /// expected scope from the catalog row instead of a discovery lease.
    // Adopted by the daemon watcher startup path; that integration is a
    // separate ownership grant and lands with it.
    #[allow(dead_code)]
    pub(crate) fn with_catalog(
        broker: Arc<CheckoutAccessBroker>,
        catalog: Option<Arc<ProjectCatalogStore>>,
    ) -> Self {
        Self { broker, catalog }
    }

    fn catalog_scope(&self, project_id: &str) -> Option<PublishedScope> {
        let state = self.catalog.as_ref()?.snapshot().ok()?;
        let parsed = ProjectId::parse(project_id).ok()?;
        match &state.catalog().projects.get(&parsed)?.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal => None,
        }
    }
}

/// The active attachments that carry `artifact_watching`, as native watcher
/// carriers (plan section 8, P5-F watcher item 2).
///
/// An attachment without the capability yields no carrier at all: the plan's
/// degradation for this row is "no watcher plus bounded capability health",
/// not a registration that fails on every event. Durable artifact metadata
/// already in the catalog is unaffected either way.
#[allow(dead_code)] // See `with_catalog`: startup/observer integration is a separate grant.
pub(crate) fn catalog_watch_carriers(state: &SharedState) -> Vec<ArtifactWatchCarrier> {
    let Some(store) = state.project_authority.catalog_store() else {
        return Vec::new();
    };
    let Ok(snapshot) = store.snapshot() else {
        tracing::warn!("catalog snapshot unavailable; watcher reconciliation skipped this pass");
        return Vec::new();
    };
    snapshot
        .attachments()
        .attachments
        .values()
        .filter(|attachment| attachment.status == AttachmentStatus::Attached)
        .filter(|attachment| attachment.capabilities.artifact_watching)
        .filter_map(|attachment| {
            ArtifactWatchCarrier::for_attachment(
                attachment.project_id.as_str(),
                attachment.attachment_id.as_str(),
            )
            .map_err(|error| {
                tracing::warn!(
                    project = %attachment.project_id.as_str(),
                    error = %error,
                    "artifact watcher rejected a catalog attachment carrier"
                );
            })
            .ok()
        })
        .collect()
}

/// Reconcile the live watcher's native registrations against the catalog.
///
/// Safe to call for every post-commit event including duplicates: the
/// reconciler compares desired against installed, so an unchanged catalog
/// produces no watch churn. A project with no watcher (no daemon watcher
/// yet, or none of its attachments capable) is not an error here.
#[allow(dead_code)] // See `with_catalog`: startup/observer integration is a separate grant.
pub(crate) fn reconcile_catalog_watchers(state: &SharedState) {
    if state.project_authority.catalog_store().is_none() {
        return;
    }
    let desired = catalog_watch_carriers(state);
    let Ok(mut guard) = state.bbox_watcher.lock() else {
        return;
    };
    let Some(watcher) = guard.as_mut() else {
        return;
    };
    let report = watcher.reconcile_attachment_registrations(&desired);
    if !report.is_noop() || report.failed > 0 {
        tracing::debug!(
            added = report.added,
            removed = report.removed,
            relocated = report.relocated,
            failed = report.failed,
            "artifact watcher reconciled catalog registrations"
        );
    }
}

impl ArtifactWatchAccess for DaemonArtifactWatchAccess {
    fn with_discovery(
        &self,
        carrier: &ArtifactWatchCarrier,
        prepare: &mut dyn FnMut(&dyn ArtifactWatchRead) -> Result<()>,
        publish: &mut dyn FnMut(&dyn ArtifactWatchRead) -> Result<()>,
    ) -> Result<()> {
        // A native attachment carries the catalog's scope on its own row, so
        // it needs no scope-discovery lease. Taking one anyway would gate
        // artifact watching on `repo_knowledge` (D-032), which the section 9
        // table assigns to the publisher and overlay lanes, not to this one.
        // The bridge carriers keep the two-step verbatim: a version-1 record
        // carries no scope at all.
        let (attachment, expected_scope, source_lane) = match carrier.attachment() {
            ArtifactWatchAttachment::AttachmentId(attachment_id) => (
                CheckoutAttachmentSelector::AttachmentId(attachment_id.clone()),
                self.catalog_scope(carrier.project_id()),
                CheckoutAccessSourceLane::NativeAttachment,
            ),
            legacy => {
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
                match legacy {
                    ArtifactWatchAttachment::Selected => (
                        CheckoutAttachmentSelector::Selected,
                        expected_scope,
                        CheckoutAccessSourceLane::LegacyProjectRecord,
                    ),
                    ArtifactWatchAttachment::CheckoutId(checkout_id) => (
                        CheckoutAttachmentSelector::CheckoutId(checkout_id.clone()),
                        expected_scope,
                        CheckoutAccessSourceLane::LegacyCheckoutRegistry,
                    ),
                    ArtifactWatchAttachment::AttachmentId(_) => unreachable!("handled above"),
                }
            }
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

#[cfg(test)]
mod tests {
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, CheckoutAttachment,
    };

    use super::*;
    use crate::server::state::catalog_fixture::CatalogFixture;

    const PROJECT: &str = "proj_watch";
    const CAPABLE: &str = "att_00000000000000000000000000000c01";
    const INCAPABLE: &str = "att_00000000000000000000000000000c02";
    const CHECKOUT_ONE: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb01";
    const CHECKOUT_TWO: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb02";

    /// Insert one attachment with exactly the capability bits under test.
    ///
    /// Built here rather than in the shared fixture: the fixture's attach
    /// helpers fix `repo_knowledge` and this lane turns on
    /// `artifact_watching` instead.
    fn attach(
        server: &BlackboxServer,
        attachment_id: &str,
        checkout_id: &str,
        checkout_dir: &std::path::Path,
        capabilities: AttachmentCapabilities,
        status: AttachmentStatus,
    ) {
        std::fs::create_dir_all(checkout_dir.join(".bbox/local")).unwrap();
        std::fs::write(
            checkout_dir.join(".bbox/local/checkout-id"),
            format!("{checkout_id}\n"),
        )
        .unwrap();
        let store = server
            .state
            .project_authority
            .catalog_store()
            .expect("catalog authority");
        let scope = CatalogFixture::scope(".");
        let project_id = ProjectId::parse(PROJECT).unwrap();
        let attachment_id = AttachmentId::parse(attachment_id).unwrap();
        let checkout_dir = checkout_dir.to_string_lossy().into_owned();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_catalog, attachments| {
                attachments.attachments.insert(
                    attachment_id.clone(),
                    CheckoutAttachment {
                        attachment_id: attachment_id.clone(),
                        project_id: project_id.clone(),
                        checkout_id: checkout_id.to_string(),
                        checkout_dir: checkout_dir.clone(),
                        checkout_project_dir: checkout_dir.clone(),
                        project_root_relpath: ".".into(),
                        kind: AttachmentKind::Base,
                        validated_scope: Some(scope.clone()),
                        computed_repo_hint: None,
                        branch_ref: Some("refs/heads/main".into()),
                        capabilities,
                        status,
                        attached_at: "2026-08-03T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
    }

    fn watching() -> AttachmentCapabilities {
        AttachmentCapabilities {
            artifact_watching: true,
            ..Default::default()
        }
    }

    /// Only an active attachment recording `artifact_watching` becomes a
    /// carrier, and the carrier names the attachment id, not a checkout id
    /// or the Selected ladder.
    #[test]
    fn catalog_carriers_are_attachment_ids_gated_on_the_capability() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();

        attach(
            &server,
            CAPABLE,
            CHECKOUT_ONE,
            &root.join("capable"),
            watching(),
            AttachmentStatus::Attached,
        );
        attach(
            &server,
            INCAPABLE,
            CHECKOUT_TWO,
            &root.join("incapable"),
            // Every other bit set: the filter must key on the one this lane
            // owns, not on "has some capability".
            AttachmentCapabilities {
                artifact_watching: false,
                repo_knowledge: true,
                local_code_source: true,
                ..Default::default()
            },
            AttachmentStatus::Attached,
        );

        let carriers = catalog_watch_carriers(&server.state);
        assert_eq!(carriers.len(), 1, "{carriers:#?}");
        assert_eq!(carriers[0].project_id(), PROJECT);
        assert_eq!(
            carriers[0].attachment(),
            &ArtifactWatchAttachment::AttachmentId(CAPABLE.to_string())
        );
        assert!(carriers[0].is_attachment());
    }

    /// Detach removes the carrier. The store additionally refuses to hold a
    /// detached row that still claims capabilities, so "detached but still
    /// watching" is unrepresentable rather than merely filtered; this drives
    /// the real detach path and asserts the carrier is gone with it.
    #[test]
    fn detach_removes_the_native_carrier() {
        let fixture = CatalogFixture::new();
        fixture.add_published_project(PROJECT, &CatalogFixture::scope("."));
        let server = fixture.server();
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        attach(
            &server,
            CAPABLE,
            CHECKOUT_ONE,
            &root.join("capable"),
            watching(),
            AttachmentStatus::Attached,
        );
        assert_eq!(catalog_watch_carriers(&server.state).len(), 1);

        CatalogFixture::detach_in_server(&server, CAPABLE);

        assert!(catalog_watch_carriers(&server.state).is_empty());
    }

    /// Bridge mode has no catalog to project, so the native lane is empty
    /// and its reconciler is inert. The legacy Selected and CheckoutId
    /// registrations remain the bridge's whole watcher story.
    #[test]
    fn bridge_mode_projects_no_native_carriers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let state = crate::server::state::SharedState::for_test(&root);
        assert!(catalog_watch_carriers(&state).is_empty());
        // Inert, not a panic: a bridge daemon calls the reconciler from the
        // same post-commit path a catalog daemon does.
        reconcile_catalog_watchers(&state);
    }
}

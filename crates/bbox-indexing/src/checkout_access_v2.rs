//! Catalog-backed checkout-access authority
//! (design/daemon-runtime/durable-project-catalog-phase2-impl.md §6.3).
//!
//! Resolves broker requests against the strict catalog/attachment pair
//! instead of the version-1 registry and census. Selector semantics:
//! `Selected` and `AttachmentId` resolve through active attachments with
//! catalog cross-validation, `CheckoutId` maps through active attachments
//! for that checkout, and `LegacyPath` resolves through the shared
//! resolver's catalog path arms and keeps its compatibility-lane counting
//! so the retirement telemetry means one thing across modes.
//!
//! Request source lanes keep the version-1 selector-to-lane contract in
//! this milestone: the lane names the caller's selector origin, and the
//! call sites are converted (with native lane labels) in the routing
//! milestone, not here.
//!
//! Capabilities are real per-attachment state here: a request kind is
//! admitted only when the attachment records the matching capability, and
//! every capability is revalidated at acquisition rather than inferred
//! from directory existence (governing §5.2).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use bbox_corpus_core::project_catalog::{
    AttachmentCapabilities, AttachmentId, AttachmentStatus, ProjectId,
};
use bbox_corpus_core::project_selector::{
    ProjectSelectorRequest, ResolvedAttachment, SelectorClass,
};

use crate::checkout_access::{
    CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError, CheckoutAccessErrorCode,
    CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
    CheckoutAttachmentSelector, CheckoutAttachmentStatus,
};
use crate::project_catalog_store::{ProjectCatalogState, ProjectCatalogStore};
use crate::project_resolver::ProjectResolverEngine;
use crate::projects::ResolveIntent;

/// Strict authority over the daemon's opened catalog store. Every resolve
/// reads the current published pair snapshot; the store's epoch discipline
/// makes a stale candidate detectable at revalidation.
pub struct V2CatalogCheckoutAccessAuthority {
    store: Arc<ProjectCatalogStore>,
}

impl V2CatalogCheckoutAccessAuthority {
    pub fn new(store: Arc<ProjectCatalogStore>) -> Self {
        Self { store }
    }

    fn state(&self) -> std::result::Result<Arc<ProjectCatalogState>, CheckoutAccessError> {
        self.store.snapshot().map_err(|error| {
            access_error(
                CheckoutAccessErrorCode::ObservationUnavailable,
                &format!("catalog state unavailable: {}", error.code()),
            )
        })
    }
}

impl CheckoutAccessAuthority for V2CatalogCheckoutAccessAuthority {
    fn resolve(
        &self,
        request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        let state = self.state()?;
        resolve_candidate(request, &state)
    }

    fn revalidate_conservative_path_gate(
        &self,
        request: &CheckoutAccessRequest,
        candidate: &CheckoutAccessCandidate,
    ) -> std::result::Result<(), CheckoutAccessError> {
        // Re-resolve against the current pair so publication observes
        // detach, reattach, or relocation, then require exact agreement
        // with the canonicalized roots the lease was built from.
        let state = self.state()?;
        let refreshed = resolve_candidate(request, &state)?;
        let refreshed_checkout_root = canonical_directory(&refreshed.checkout_root)?;
        let refreshed_project_root = canonical_directory(&refreshed.project_root)?;
        if refreshed.project_id != candidate.project_id
            || refreshed.attachment_id != candidate.attachment_id
            || refreshed.checkout_id != candidate.checkout_id
            || refreshed.published_scope != candidate.published_scope
            || refreshed.branch_ref != candidate.branch_ref
            || refreshed_checkout_root != candidate.checkout_root
            || refreshed_project_root != candidate.project_root
        {
            return Err(access_error(
                CheckoutAccessErrorCode::ConservativePathGateDenied,
                "catalog attachment authority changed while access was being validated",
            ));
        }
        Ok(())
    }
}

fn resolve_candidate(
    request: &CheckoutAccessRequest,
    state: &ProjectCatalogState,
) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
    validate_source_lane(request)?;
    let catalog = state.catalog();
    let attachments = state.attachments();
    let attachment = match &request.attachment {
        CheckoutAttachmentSelector::Selected => {
            let project_id = parse_project_id(&request.project_id)?;
            if !catalog.projects.contains_key(&project_id) {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "project is not in the catalog",
                ));
            }
            let mut active = attachments.attachments.values().filter(|row| {
                row.status == AttachmentStatus::Attached && row.project_id == project_id
            });
            let Some(first) = active.next() else {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "project has no active attachment",
                ));
            };
            if active.next().is_some() {
                return Err(access_error(
                    CheckoutAccessErrorCode::SelectorMismatch,
                    "project has multiple active attachments; select one explicitly",
                ));
            }
            first
        }
        CheckoutAttachmentSelector::AttachmentId(raw) => {
            let id = AttachmentId::parse(raw.as_str()).map_err(|_| {
                access_error(
                    CheckoutAccessErrorCode::InvalidRequest,
                    "attachment id is malformed",
                )
            })?;
            let Some(row) = attachments.attachments.get(&id) else {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "attachment id is not in the attachment store",
                ));
            };
            if !request.project_id.is_empty() {
                let project_id = parse_project_id(&request.project_id)?;
                if row.project_id != project_id {
                    return Err(access_error(
                        CheckoutAccessErrorCode::ProjectMismatch,
                        "attachment belongs to a different project",
                    ));
                }
            }
            row
        }
        CheckoutAttachmentSelector::CheckoutId(checkout_id) => {
            let mut rows = attachments.attachments.values().filter(|row| {
                row.status == AttachmentStatus::Attached
                    && row.checkout_id == *checkout_id
                    && (request.project_id.is_empty()
                        || ProjectId::parse(&request.project_id)
                            .is_ok_and(|id| row.project_id == id))
            });
            let Some(first) = rows.next() else {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "no active attachment carries this checkout id",
                ));
            };
            if rows.next().is_some() {
                return Err(access_error(
                    CheckoutAccessErrorCode::SelectorMismatch,
                    "checkout id is ambiguous across active attachments",
                ));
            }
            first
        }
        CheckoutAttachmentSelector::LegacyPath(raw) => {
            let engine = ProjectResolverEngine::v2(catalog, attachments);
            let intent = match request.intent {
                CheckoutAccessIntent::Read => ResolveIntent::Read,
                CheckoutAccessIntent::Write => ResolveIntent::Write,
            };
            let mut selector_request = ProjectSelectorRequest::selection(raw.clone(), intent);
            selector_request.class = SelectorClass::Selection;
            let resolved = engine
                .resolve_attached(&selector_request)
                .map_err(|error| {
                    let code = match error.code() {
                        "error.project_selector_ambiguous" => {
                            CheckoutAccessErrorCode::SelectorMismatch
                        }
                        _ => CheckoutAccessErrorCode::AttachmentNotFound,
                    };
                    access_error(code, error.detail())
                })?;
            let ResolvedAttachment::Catalog { attachment_id, .. } = &resolved.attachment else {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "catalog resolution did not select a catalog attachment",
                ));
            };
            let id = AttachmentId::parse(attachment_id.as_str()).map_err(|_| {
                access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "resolved attachment id is malformed",
                )
            })?;
            let Some(row) = attachments.attachments.get(&id) else {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "resolved attachment is not in the attachment store",
                ));
            };
            row
        }
    };

    if attachment.status != AttachmentStatus::Attached {
        return Err(access_error(
            CheckoutAccessErrorCode::AttachmentInactive,
            "attachment is detached",
        ));
    }
    if let Some(expected) = &request.expected_scope
        && attachment.validated_scope.as_ref() != Some(expected)
    {
        return Err(access_error(
            CheckoutAccessErrorCode::ScopeMismatch,
            "attachment scope disagrees with the expected scope",
        ));
    }
    if !capability_admits(&attachment.capabilities, request.kind) {
        return Err(access_error(
            CheckoutAccessErrorCode::CapabilityDenied,
            "attachment does not record the required capability",
        ));
    }

    Ok(CheckoutAccessCandidate {
        project_id: attachment.project_id.as_str().to_string(),
        attachment_id: attachment.attachment_id.as_str().to_string(),
        checkout_id: attachment.checkout_id.clone(),
        published_scope: attachment.validated_scope.clone(),
        branch_ref: attachment.branch_ref.clone(),
        checkout_root: PathBuf::from(&attachment.checkout_dir),
        project_root: PathBuf::from(&attachment.checkout_project_dir),
        status: CheckoutAttachmentStatus::Active,
        capabilities: BTreeSet::from([request.kind]),
        lifetime_guard: None,
    })
}

fn parse_project_id(raw: &str) -> std::result::Result<ProjectId, CheckoutAccessError> {
    ProjectId::parse(raw).map_err(|_| {
        access_error(
            CheckoutAccessErrorCode::InvalidRequest,
            "project id is malformed",
        )
    })
}

/// The nine access kinds against the eight recorded capabilities: the
/// publisher/config tree read and the knowledge/gap overlay read both ride
/// the repo-knowledge capability; every other kind maps one-to-one.
fn capability_admits(capabilities: &AttachmentCapabilities, kind: CheckoutAccessKind) -> bool {
    match kind {
        CheckoutAccessKind::LocalProjectWalk => capabilities.local_code_source,
        CheckoutAccessKind::GitHistory => capabilities.git_history,
        CheckoutAccessKind::PublisherConfigTreeRead => capabilities.repo_knowledge,
        CheckoutAccessKind::KnowledgeGapOverlayRead => capabilities.repo_knowledge,
        CheckoutAccessKind::Blame => capabilities.blame,
        CheckoutAccessKind::RenderFileProvider => capabilities.render_output,
        CheckoutAccessKind::ProvenanceNoteIo => capabilities.provenance_note_io,
        CheckoutAccessKind::ArtifactWatchDiscovery => capabilities.artifact_watching,
        CheckoutAccessKind::RepositoryMutation => capabilities.repo_mutation,
    }
}

/// Same request-side contract as the version-1 authority: the lane names
/// the caller's selector origin. Native relabeling of converted call sites
/// happens in the routing milestone.
fn validate_source_lane(
    request: &CheckoutAccessRequest,
) -> std::result::Result<(), CheckoutAccessError> {
    let expected = match &request.attachment {
        CheckoutAttachmentSelector::Selected => CheckoutAccessSourceLane::LegacyProjectRecord,
        CheckoutAttachmentSelector::CheckoutId(_) => {
            CheckoutAccessSourceLane::LegacyCheckoutRegistry
        }
        CheckoutAttachmentSelector::AttachmentId(_) => CheckoutAccessSourceLane::NativeAttachment,
        CheckoutAttachmentSelector::LegacyPath(_) => CheckoutAccessSourceLane::LegacyPathResolver,
    };
    if request.source_lane != expected {
        return Err(access_error(
            CheckoutAccessErrorCode::InvalidRequest,
            "checkout selector does not match its bounded observation source lane",
        ));
    }
    Ok(())
}

fn canonical_directory(path: &Path) -> std::result::Result<PathBuf, CheckoutAccessError> {
    let canonical = std::fs::canonicalize(path).map_err(|_| {
        access_error(
            CheckoutAccessErrorCode::AttachmentInactive,
            "checkout authority root cannot be canonicalized",
        )
    })?;
    if !canonical.is_dir() {
        return Err(access_error(
            CheckoutAccessErrorCode::AttachmentInactive,
            "checkout authority root is not a directory",
        ));
    }
    Ok(canonical)
}

fn access_error(code: CheckoutAccessErrorCode, diagnostic: &str) -> CheckoutAccessError {
    CheckoutAccessError::new(code, diagnostic)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        AttachmentKind, CatalogSnapshotV2, CheckoutAttachment, CorpusProject, ProjectScope,
    };

    const PROJECT: &str = "p_00000000000000000000000000000a01";
    const ATTACHMENT: &str = "att_0000000000000000000000000000a001";
    const CHECKOUT: &str = "feed00000000000000000000000000a1";

    fn store_with_fixture(root: &Path) -> Arc<ProjectCatalogStore> {
        let projects_path = root.join("projects.json");
        let store = ProjectCatalogStore::initialize_empty(&projects_path).unwrap();
        let checkout_dir = root.join("checkout");
        std::fs::create_dir_all(checkout_dir.join("sub")).unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |catalog: &mut CatalogSnapshotV2, attachments| {
                catalog.projects.insert(
                    ProjectId::parse(PROJECT).unwrap(),
                    CorpusProject {
                        project_id: ProjectId::parse(PROJECT).unwrap(),
                        scope: ProjectScope::LegacyLocal,
                        operator_aliases: Default::default(),
                        nominated_aliases: Default::default(),
                        display_name: "fixture".into(),
                        created_at: "2026-07-24T00:00:00Z".into(),
                        registered_at_compat: None,
                        repo_history: None,
                        languages: Default::default(),
                    },
                );
                attachments.attachments.insert(
                    AttachmentId::parse(ATTACHMENT).unwrap(),
                    CheckoutAttachment {
                        attachment_id: AttachmentId::parse(ATTACHMENT).unwrap(),
                        project_id: ProjectId::parse(PROJECT).unwrap(),
                        checkout_id: CHECKOUT.into(),
                        checkout_dir: checkout_dir.to_str().unwrap().into(),
                        checkout_project_dir: checkout_dir.to_str().unwrap().into(),
                        project_root_relpath: ".".into(),
                        kind: AttachmentKind::Base,
                        validated_scope: None,
                        computed_repo_hint: None,
                        branch_ref: None,
                        capabilities: AttachmentCapabilities {
                            local_code_source: true,
                            git_history: false,
                            blame: false,
                            repo_knowledge: true,
                            repo_mutation: false,
                            render_output: false,
                            provenance_note_io: false,
                            artifact_watching: false,
                        },
                        status: AttachmentStatus::Attached,
                        attached_at: "2026-07-24T00:00:00Z".into(),
                        detached_at: None,
                    },
                );
                Ok(())
            })
            .unwrap();
        Arc::new(store)
    }

    fn request(
        selector: CheckoutAttachmentSelector,
        lane: CheckoutAccessSourceLane,
        kind: CheckoutAccessKind,
    ) -> CheckoutAccessRequest {
        CheckoutAccessRequest {
            project_id: PROJECT.into(),
            attachment: selector,
            expected_scope: None,
            kind,
            intent: CheckoutAccessIntent::Read,
            source_lane: lane,
        }
    }

    #[test]
    fn selected_attachment_id_checkout_id_and_path_all_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store_with_fixture(&root);
        let authority = V2CatalogCheckoutAccessAuthority::new(store);

        for (selector, lane) in [
            (
                CheckoutAttachmentSelector::Selected,
                CheckoutAccessSourceLane::LegacyProjectRecord,
            ),
            (
                CheckoutAttachmentSelector::AttachmentId(ATTACHMENT.into()),
                CheckoutAccessSourceLane::NativeAttachment,
            ),
            (
                CheckoutAttachmentSelector::CheckoutId(CHECKOUT.into()),
                CheckoutAccessSourceLane::LegacyCheckoutRegistry,
            ),
            (
                CheckoutAttachmentSelector::LegacyPath(
                    root.join("checkout/sub").to_str().unwrap().into(),
                ),
                CheckoutAccessSourceLane::LegacyPathResolver,
            ),
        ] {
            let candidate = authority
                .resolve(&request(
                    selector,
                    lane,
                    CheckoutAccessKind::LocalProjectWalk,
                ))
                .unwrap();
            assert_eq!(candidate.project_id, PROJECT);
            assert_eq!(candidate.attachment_id, ATTACHMENT);
            assert_eq!(candidate.checkout_id, CHECKOUT);
            authority
                .revalidate_conservative_path_gate(
                    &request(
                        CheckoutAttachmentSelector::Selected,
                        CheckoutAccessSourceLane::LegacyProjectRecord,
                        CheckoutAccessKind::LocalProjectWalk,
                    ),
                    &candidate,
                )
                .unwrap();
        }
    }

    #[test]
    fn capability_gate_scope_gate_and_detach_deny() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store_with_fixture(&root);
        let authority = V2CatalogCheckoutAccessAuthority::new(store.clone());

        // Missing capability bit denies.
        let err = authority
            .resolve(&request(
                CheckoutAttachmentSelector::Selected,
                CheckoutAccessSourceLane::LegacyProjectRecord,
                CheckoutAccessKind::RepositoryMutation,
            ))
            .unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::CapabilityDenied);

        // The repo-knowledge capability admits both mapped kinds.
        for kind in [
            CheckoutAccessKind::PublisherConfigTreeRead,
            CheckoutAccessKind::KnowledgeGapOverlayRead,
        ] {
            authority
                .resolve(&request(
                    CheckoutAttachmentSelector::Selected,
                    CheckoutAccessSourceLane::LegacyProjectRecord,
                    kind,
                ))
                .unwrap();
        }

        // Expected-scope disagreement denies (attachment has no scope).
        let mut scoped = request(
            CheckoutAttachmentSelector::Selected,
            CheckoutAccessSourceLane::LegacyProjectRecord,
            CheckoutAccessKind::LocalProjectWalk,
        );
        scoped.expected_scope =
            Some(bbox_corpus_core::identity::PublishedScope::try_new("somefamily", ".").unwrap());
        let err = authority.resolve(&scoped).unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::ScopeMismatch);

        // Detach denies every selector.
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                let row = attachments
                    .attachments
                    .get_mut(&AttachmentId::parse(ATTACHMENT).unwrap())
                    .unwrap();
                row.status = AttachmentStatus::Detached;
                row.detached_at = Some("2026-07-24T00:00:01Z".into());
                // Strict validation: a detached row may not claim active
                // capability state (the detach operation clears it).
                row.capabilities = AttachmentCapabilities::default();
                Ok(())
            })
            .unwrap();
        let err = authority
            .resolve(&request(
                CheckoutAttachmentSelector::Selected,
                CheckoutAccessSourceLane::LegacyProjectRecord,
                CheckoutAccessKind::LocalProjectWalk,
            ))
            .unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::AttachmentNotFound);
    }

    #[test]
    fn revalidation_detects_detach_between_resolve_and_gate() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store_with_fixture(&root);
        let authority = V2CatalogCheckoutAccessAuthority::new(store.clone());
        let req = request(
            CheckoutAttachmentSelector::Selected,
            CheckoutAccessSourceLane::LegacyProjectRecord,
            CheckoutAccessKind::LocalProjectWalk,
        );
        let candidate = authority.resolve(&req).unwrap();

        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                let row = attachments
                    .attachments
                    .get_mut(&AttachmentId::parse(ATTACHMENT).unwrap())
                    .unwrap();
                row.status = AttachmentStatus::Detached;
                row.detached_at = Some("2026-07-24T00:00:01Z".into());
                // Strict validation: a detached row may not claim active
                // capability state (the detach operation clears it).
                row.capabilities = AttachmentCapabilities::default();
                Ok(())
            })
            .unwrap();

        let err = authority
            .revalidate_conservative_path_gate(&req, &candidate)
            .unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::AttachmentNotFound);
    }
}

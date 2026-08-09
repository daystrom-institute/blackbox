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
    AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
    ProjectId, ProjectScope,
};
use bbox_corpus_core::project_selector::{
    ProjectSelectorRequest, ResolvedAttachment, SelectorClass,
};

use crate::checkout_access::{
    CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError, CheckoutAccessErrorCode,
    CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest, CheckoutAccessSourceLane,
    CheckoutAttachmentSelector, CheckoutAttachmentStatus, CheckoutRecordedProjectScope,
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

    fn recorded_project_scope(
        &self,
        project_id: &str,
    ) -> std::result::Result<CheckoutRecordedProjectScope, CheckoutAccessError> {
        let project_id = parse_project_id(project_id)?;
        let state = self.state()?;
        let project = state.catalog().projects.get(&project_id).ok_or_else(|| {
            access_error(
                CheckoutAccessErrorCode::AttachmentNotFound,
                "project is not in the catalog",
            )
        })?;
        Ok(match &project.scope {
            ProjectScope::Published(scope) => {
                CheckoutRecordedProjectScope::Published(scope.clone())
            }
            ProjectScope::LegacyLocal => CheckoutRecordedProjectScope::LegacyLocal,
        })
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
            // Selection ladder for path operations without an explicit
            // selector (plan §7.3, review M1): the operator-selected
            // default, then a single active attachment, then the unique
            // active `Base` attachment (the §5.3 key-to-base rule applied
            // to lease selection: index and overlay lanes act on the
            // durable base checkout). Session pins ride the engine and
            // arrive here as explicit `AttachmentId` selectors.
            let active: Vec<&CheckoutAttachment> = attachments
                .attachments
                .values()
                .filter(|row| {
                    row.status == AttachmentStatus::Attached && row.project_id == project_id
                })
                .collect();
            if active.is_empty() {
                return Err(access_error(
                    CheckoutAccessErrorCode::AttachmentNotFound,
                    "project has no active attachment",
                ));
            }
            let default = attachments
                .default_attachments
                .get(&project_id)
                .and_then(|selected| {
                    active
                        .iter()
                        .find(|row| &row.attachment_id == selected)
                        .copied()
                });
            let single = (active.len() == 1).then(|| active[0]);
            let unique_base = || {
                let mut bases = active.iter().filter(|row| row.kind == AttachmentKind::Base);
                match (bases.next(), bases.next()) {
                    (Some(base), None) => Some(*base),
                    _ => None,
                }
            };
            match default.or(single).or_else(unique_base) {
                Some(row) => row,
                None => {
                    return Err(access_error(
                        CheckoutAccessErrorCode::SelectorMismatch,
                        "project has multiple active attachments and no default or \
                         unique base; select one explicitly",
                    ));
                }
            }
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
    verify_live_checkout(attachment)?;

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

/// Live checkout verification at lease resolution and revalidation
/// (plan §6.3, governing §5.2, review H1): the recorded attachment must
/// still name the same on-disk checkout. Both recorded directories must
/// canonicalize to themselves (the catalog-mode conservative gate: the
/// attachment IS the aliasing authority, so a moved or replaced directory
/// denies rather than re-resolving), and the durable checkout-id marker
/// must match the recorded identity exactly. Path existence and inode
/// reuse never prove sameness; every v2 attachment minted its marker at
/// attach time, so a missing or divergent marker is identity loss and
/// fails closed for every intent.
fn verify_live_checkout(
    attachment: &CheckoutAttachment,
) -> std::result::Result<(), CheckoutAccessError> {
    let recorded_checkout = Path::new(&attachment.checkout_dir);
    let checkout_root = canonical_directory(recorded_checkout)?;
    if checkout_root != recorded_checkout {
        return Err(access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "recorded checkout dir no longer canonicalizes to itself",
        ));
    }
    let recorded_project = Path::new(&attachment.checkout_project_dir);
    let project_root = canonical_directory(recorded_project)?;
    if project_root != recorded_project {
        return Err(access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "recorded project dir no longer canonicalizes to itself",
        ));
    }
    let marker = checkout_root.join(".bbox/local/checkout-id");
    match bbox_corpus_core::identity::read_checkout_id(&marker) {
        Ok(Some(found)) if found == attachment.checkout_id => Ok(()),
        Ok(Some(_)) => Err(access_error(
            CheckoutAccessErrorCode::CheckoutIdentityMismatch,
            "checkout identity marker names a different checkout",
        )),
        Ok(None) => Err(access_error(
            CheckoutAccessErrorCode::CheckoutIdentityMismatch,
            "checkout identity marker is missing",
        )),
        Err(_) => Err(access_error(
            CheckoutAccessErrorCode::CheckoutIdentityMismatch,
            "checkout identity marker is unreadable",
        )),
    }
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
        // Every v2 attachment minted its durable identity at attach time;
        // live verification reads it back on each lease.
        std::fs::create_dir_all(checkout_dir.join(".bbox/local")).unwrap();
        std::fs::write(
            checkout_dir.join(".bbox/local/checkout-id"),
            format!("{CHECKOUT}\n"),
        )
        .unwrap();
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

    /// Review H1: the live checkout must still prove the recorded identity
    /// at resolve and revalidation time. A rewritten marker (same inode
    /// directory) and a missing marker both fail closed.
    #[test]
    fn live_checkout_verification_denies_marker_drift_and_loss() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store_with_fixture(&root);
        let authority = V2CatalogCheckoutAccessAuthority::new(store);
        let req = request(
            CheckoutAttachmentSelector::Selected,
            CheckoutAccessSourceLane::LegacyProjectRecord,
            CheckoutAccessKind::LocalProjectWalk,
        );
        let candidate = authority.resolve(&req).unwrap();

        // Marker rewritten in place: resolve and revalidate both deny.
        let marker = root.join("checkout/.bbox/local/checkout-id");
        std::fs::write(&marker, "feed0000000000000000000000000bad\n").unwrap();
        let err = authority.resolve(&req).unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::CheckoutIdentityMismatch);
        let err = authority
            .revalidate_conservative_path_gate(&req, &candidate)
            .unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::CheckoutIdentityMismatch);

        // Marker removed: identity loss, still fails closed.
        std::fs::remove_file(&marker).unwrap();
        let err = authority.resolve(&req).unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::CheckoutIdentityMismatch);
    }

    /// Review M1: the `Selected` ladder resolves the operator default, then
    /// a single active attachment, then the unique active base; only a
    /// topology with none of those refuses.
    #[test]
    fn selected_ladder_default_then_single_then_unique_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let store = store_with_fixture(&root);

        // Add a worktree-kind attachment with its own live checkout.
        const WT_ATTACHMENT: &str = "att_0000000000000000000000000000a002";
        const WT_CHECKOUT: &str = "feed00000000000000000000000000a2";
        let wt_dir = root.join("worktree");
        std::fs::create_dir_all(wt_dir.join(".bbox/local")).unwrap();
        std::fs::write(
            wt_dir.join(".bbox/local/checkout-id"),
            format!("{WT_CHECKOUT}\n"),
        )
        .unwrap();
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                let base = attachments
                    .attachments
                    .get(&AttachmentId::parse(ATTACHMENT).unwrap())
                    .unwrap()
                    .clone();
                attachments.attachments.insert(
                    AttachmentId::parse(WT_ATTACHMENT).unwrap(),
                    CheckoutAttachment {
                        attachment_id: AttachmentId::parse(WT_ATTACHMENT).unwrap(),
                        checkout_id: WT_CHECKOUT.into(),
                        checkout_dir: wt_dir.to_str().unwrap().into(),
                        checkout_project_dir: wt_dir.to_str().unwrap().into(),
                        kind: AttachmentKind::Worktree,
                        ..base
                    },
                );
                Ok(())
            })
            .unwrap();
        let authority = V2CatalogCheckoutAccessAuthority::new(store.clone());
        let req = request(
            CheckoutAttachmentSelector::Selected,
            CheckoutAccessSourceLane::LegacyProjectRecord,
            CheckoutAccessKind::LocalProjectWalk,
        );

        // Base + worktree with no default: the unique base wins (the
        // key-to-base rule applied to lease selection).
        let candidate = authority.resolve(&req).unwrap();
        assert_eq!(candidate.attachment_id, ATTACHMENT);

        // An operator default outranks the base rung.
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                attachments.default_attachments.insert(
                    ProjectId::parse(PROJECT).unwrap(),
                    AttachmentId::parse(WT_ATTACHMENT).unwrap(),
                );
                Ok(())
            })
            .unwrap();
        let candidate = authority.resolve(&req).unwrap();
        assert_eq!(candidate.attachment_id, WT_ATTACHMENT);

        // Two worktrees, no base, no default: fail closed.
        let epoch = store.snapshot().unwrap().epoch();
        store
            .transact(epoch, |_, attachments| {
                attachments.default_attachments.clear();
                let row = attachments
                    .attachments
                    .get_mut(&AttachmentId::parse(ATTACHMENT).unwrap())
                    .unwrap();
                row.kind = AttachmentKind::Worktree;
                Ok(())
            })
            .unwrap();
        let err = authority.resolve(&req).unwrap_err();
        assert_eq!(err.code, CheckoutAccessErrorCode::SelectorMismatch);
    }
}

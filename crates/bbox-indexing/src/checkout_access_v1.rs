//! Version-1 project and checkout registry authority for checkout leases.
//!
//! This compatibility adapter is reusable by daemon and indexing consumers.
//! It stores shared registry handles only. Raw paths remain inside the legacy
//! authority stores, short-lived candidates, and validated leases.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use bbox_corpus_core::identity::{PublishedScope, ensure_checkout_id, read_checkout_id};
use bbox_corpus_core::project_record::{ProjectContext, ProjectRecord};
use parking_lot::RwLock;
use sha2::{Digest, Sha256};

use crate::checkout_access::{
    CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError, CheckoutAccessErrorCode,
    CheckoutAccessIntent, CheckoutAccessRequest, CheckoutAccessSourceLane,
    CheckoutAttachmentSelector, CheckoutAttachmentStatus,
};
use crate::checkout_registry::{CheckoutRegistry, CheckoutRow};
use crate::projects::{ProjectRegistry, ResolveIntent, resolve_project_context};

/// Compatibility authority over the shared version-1 stores.
pub struct V1CheckoutAccessAuthority {
    projects: Arc<RwLock<ProjectRegistry>>,
    checkouts: Arc<RwLock<CheckoutRegistry>>,
}

impl V1CheckoutAccessAuthority {
    pub fn new(
        projects: Arc<RwLock<ProjectRegistry>>,
        checkouts: Arc<RwLock<CheckoutRegistry>>,
    ) -> Self {
        Self {
            projects,
            checkouts,
        }
    }
}

impl CheckoutAccessAuthority for V1CheckoutAccessAuthority {
    fn resolve(
        &self,
        request: &CheckoutAccessRequest,
    ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
        let projects = self.projects.read().list();
        let checkouts = self.checkouts.read().rows().to_vec();
        resolve_candidate(request, &projects, &checkouts)
    }

    fn revalidate_conservative_path_gate(
        &self,
        request: &CheckoutAccessRequest,
        candidate: &CheckoutAccessCandidate,
    ) -> std::result::Result<(), CheckoutAccessError> {
        let projects = self.projects.read().list();
        let checkouts = self.checkouts.read().rows().to_vec();
        revalidate_candidate(request, candidate, &projects, &checkouts)
    }
}

fn revalidate_candidate(
    request: &CheckoutAccessRequest,
    candidate: &CheckoutAccessCandidate,
    projects: &[ProjectRecord],
    checkouts: &[CheckoutRow],
) -> std::result::Result<(), CheckoutAccessError> {
    // Resolve the selector again so read-side publication observes detach
    // or relocation without retaining registry locks across filesystem I/O.
    let refreshed = resolve_candidate(request, projects, checkouts)?;
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
            "checkout authority changed while access was being validated",
        ));
    }

    if request.intent == CheckoutAccessIntent::Write
        || !matches!(
            request.attachment,
            CheckoutAttachmentSelector::CheckoutId(_)
        )
    {
        let intent = match request.intent {
            CheckoutAccessIntent::Read => ResolveIntent::Read,
            CheckoutAccessIntent::Write => ResolveIntent::Write,
        };
        let roots = resolution_roots(
            &candidate.project_id,
            &candidate.project_root,
            projects,
            intent,
        )?;
        if roots.checkout_root != candidate.checkout_root
            || roots.project_root != candidate.project_root
        {
            return Err(access_error(
                CheckoutAccessErrorCode::ConservativePathGateDenied,
                "project resolver returned different roots during access validation",
            ));
        }
    }
    Ok(())
}

fn resolve_candidate(
    request: &CheckoutAccessRequest,
    projects: &[ProjectRecord],
    checkouts: &[CheckoutRow],
) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
    validate_source_lane(request)?;
    let mut candidate = match &request.attachment {
        CheckoutAttachmentSelector::Selected => resolve_selected(request, projects)?,
        CheckoutAttachmentSelector::AttachmentId(_) => {
            return Err(access_error(
                CheckoutAccessErrorCode::AttachmentNotFound,
                "version-1 authority cannot resolve a catalog attachment id",
            ));
        }
        CheckoutAttachmentSelector::CheckoutId(checkout_id) => {
            let rows = checkouts
                .iter()
                .filter(|row| row.checkout_id == *checkout_id)
                .cloned()
                .collect::<Vec<_>>();
            resolve_checkout(request, checkout_id, rows, projects)?
        }
        CheckoutAttachmentSelector::LegacyPath(raw) => resolve_legacy_path(request, raw, projects)?,
    };
    let requested_capability_is_safe = match request.intent {
        CheckoutAccessIntent::Read => true,
        CheckoutAccessIntent::Write => resolution_roots(
            &candidate.project_id,
            &candidate.project_root,
            projects,
            ResolveIntent::Write,
        )
        .is_ok_and(|roots| {
            roots.checkout_root == candidate.checkout_root
                && roots.project_root == candidate.project_root
        }),
    };
    candidate.capabilities = requested_capability_is_safe
        .then(|| BTreeSet::from([request.kind]))
        .unwrap_or_default();
    Ok(candidate)
}

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

fn resolve_selected(
    request: &CheckoutAccessRequest,
    projects: &[ProjectRecord],
) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
    let project = unique_project(&request.project_id, projects)?;
    let project_root = PathBuf::from(&project.canonical_path);
    if !project_root.is_dir() {
        return Err(access_error(
            CheckoutAccessErrorCode::AttachmentInactive,
            "selected version-1 project root is unavailable",
        ));
    }
    let project_root = canonical_directory(&project_root)?;
    let checkout_root = bbox_corpus_core::git::git_root_for_path(&project_root)
        .unwrap_or_else(|| project_root.clone());
    let checkout_root = canonical_directory(&checkout_root)?;
    let published_scope = project_scope(project);
    let marker = checkout_root.join(".bbox/local/checkout-id");
    let checkout_id = match read_checkout_id(&marker) {
        Ok(Some(value)) if bounded_non_path_id(&value) => value,
        Ok(Some(_)) | Err(_) => {
            return Err(access_error(
                CheckoutAccessErrorCode::CheckoutIdentityMismatch,
                "selected checkout identity marker is invalid",
            ));
        }
        Ok(None) => deterministic_id("v1-root", &[&project.project_id]),
    };
    let attachment_id = deterministic_id(
        "v1-attachment",
        &[&project.project_id, checkout_id.as_str()],
    );
    Ok(CheckoutAccessCandidate {
        project_id: project.project_id.clone(),
        attachment_id,
        checkout_id,
        published_scope,
        branch_ref: current_branch_ref(&checkout_root),
        checkout_root,
        project_root,
        status: CheckoutAttachmentStatus::Active,
        capabilities: BTreeSet::new(),
        lifetime_guard: None,
    })
}

fn resolve_checkout(
    request: &CheckoutAccessRequest,
    checkout_id: &str,
    rows: Vec<CheckoutRow>,
    projects: &[ProjectRecord],
) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
    if rows.is_empty() {
        return Err(access_error(
            CheckoutAccessErrorCode::AttachmentNotFound,
            "checkout id is not registered",
        ));
    }
    let expected_scope = request.expected_scope.as_ref().ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::ScopeMismatch,
            "checkout-id selection requires an exact published scope",
        )
    })?;
    let mut matching = rows
        .into_iter()
        .filter(|row| row.published_scope().as_ref() == Some(expected_scope))
        .collect::<Vec<_>>();
    if matching.is_empty() {
        return Err(access_error(
            CheckoutAccessErrorCode::ScopeMismatch,
            "checkout id is not registered for the requested scope",
        ));
    }
    if matching.len() != 1 {
        return Err(access_error(
            CheckoutAccessErrorCode::SelectorMismatch,
            "checkout id and scope resolve to more than one registry row",
        ));
    }
    let row = matching.pop().expect("one matching checkout row");
    if row
        .project_id
        .as_deref()
        .is_some_and(|project_id| project_id != request.project_id)
    {
        return Err(access_error(
            CheckoutAccessErrorCode::ProjectMismatch,
            "checkout row belongs to a different logical project",
        ));
    }
    let project = unique_project(&request.project_id, projects)?;

    let checkout_root = PathBuf::from(&row.checkout_dir);
    if !checkout_root.is_dir() {
        return Err(access_error(
            CheckoutAccessErrorCode::AttachmentInactive,
            "registered checkout root is unavailable",
        ));
    }
    let checkout_root = canonical_directory(&checkout_root)?;
    let marker = checkout_root.join(".bbox/local/checkout-id");
    if read_checkout_id(&marker).ok().flatten().as_deref() != Some(checkout_id) {
        return Err(access_error(
            CheckoutAccessErrorCode::CheckoutIdentityMismatch,
            "registered checkout identity marker is missing or changed",
        ));
    }
    let project_root = join_scope_relpath(&checkout_root, &expected_scope.bbox_root_relpath)?;
    let project_root = canonical_directory(&project_root)?;
    let attachment_id = deterministic_id(
        "v1-attachment",
        &[
            &project.project_id,
            checkout_id,
            &expected_scope.repo_id,
            &expected_scope.bbox_root_relpath,
        ],
    );
    Ok(CheckoutAccessCandidate {
        project_id: project.project_id.clone(),
        attachment_id,
        checkout_id: checkout_id.to_string(),
        published_scope: Some(expected_scope.clone()),
        branch_ref: row.branch_ref.clone(),
        checkout_root,
        project_root,
        status: CheckoutAttachmentStatus::Active,
        capabilities: BTreeSet::new(),
        lifetime_guard: None,
    })
}

fn resolve_legacy_path(
    request: &CheckoutAccessRequest,
    raw: &str,
    projects: &[ProjectRecord],
) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
    let requested_intent = match request.intent {
        CheckoutAccessIntent::Read => ResolveIntent::Read,
        CheckoutAccessIntent::Write => ResolveIntent::Write,
    };
    let context = resolve_project_context(raw, projects, requested_intent).or_else(|| {
        if requested_intent != ResolveIntent::Write {
            return None;
        }
        resolve_project_context(raw, projects, ResolveIntent::Read)
            .filter(|context| context.checkout.is_none())
    });
    let context = context.ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::AttachmentNotFound,
            "legacy path does not resolve to a registered writable project or checkout",
        )
    })?;
    let project = if context.checkout.is_some() {
        select_scope_project(raw, &context, projects)?
    } else {
        unique_project(&context.project_id, projects)?
    };

    let base_project_root = canonical_directory(Path::new(&project.canonical_path))?;
    let base_checkout_root = bbox_corpus_core::git::git_root_for_path(&base_project_root)
        .unwrap_or_else(|| base_project_root.clone());
    let base_checkout_root = canonical_directory(&base_checkout_root)?;
    let relative_project_root = base_project_root
        .strip_prefix(&base_checkout_root)
        .map_err(|_| {
            access_error(
                CheckoutAccessErrorCode::ConservativePathGateDenied,
                "registered project root is outside its checkout root",
            )
        })?;
    let checkout_root = context
        .checkout
        .as_ref()
        .map(|checkout| canonical_directory(Path::new(&checkout.checkout_dir)))
        .transpose()?
        .unwrap_or(base_checkout_root);
    let project_root = if relative_project_root.as_os_str().is_empty() {
        checkout_root.clone()
    } else {
        canonical_directory(&checkout_root.join(relative_project_root))?
    };
    let roots = resolution_roots(
        &project.project_id,
        &project_root,
        projects,
        requested_intent,
    )?;
    if roots.checkout_root != checkout_root || roots.project_root != project_root {
        return Err(access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "legacy path resolution changed before lease validation",
        ));
    }

    let marker = checkout_root.join(".bbox/local/checkout-id");
    let checkout_id = match read_checkout_id(&marker) {
        Ok(Some(value)) if bounded_non_path_id(&value) => value,
        Ok(Some(_)) | Err(_) => {
            return Err(access_error(
                CheckoutAccessErrorCode::CheckoutIdentityMismatch,
                "checkout identity marker is invalid",
            ));
        }
        Ok(None) if request.intent == CheckoutAccessIntent::Write => {
            ensure_checkout_id(&checkout_root).map_err(|error| {
                access_error(
                    CheckoutAccessErrorCode::CheckoutIdentityMismatch,
                    &format!("checkout identity could not be established: {error:#}"),
                )
            })?
        }
        Ok(None) => deterministic_id("v1-root", &[&project.project_id]),
    };
    let published_scope = project_scope(project);
    let attachment_id = deterministic_id(
        "v1-attachment",
        &[&project.project_id, checkout_id.as_str()],
    );
    Ok(CheckoutAccessCandidate {
        project_id: project.project_id.clone(),
        attachment_id,
        checkout_id,
        published_scope,
        branch_ref: current_branch_ref(&checkout_root),
        checkout_root,
        project_root,
        status: CheckoutAttachmentStatus::Active,
        capabilities: BTreeSet::new(),
        lifetime_guard: None,
    })
}

fn select_scope_project<'a>(
    raw: &str,
    context: &ProjectContext,
    projects: &'a [ProjectRecord],
) -> std::result::Result<&'a ProjectRecord, CheckoutAccessError> {
    let checkout = context.checkout.as_ref().ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::SelectorMismatch,
            "scope selection requires a concrete checkout",
        )
    })?;
    let checkout_root = canonical_directory(Path::new(&checkout.checkout_dir))?;
    let raw = canonical_directory(Path::new(raw))?;
    let raw_rel = raw.strip_prefix(&checkout_root).map_err(|_| {
        access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "legacy path is outside its resolved checkout",
        )
    })?;
    let common = bbox_corpus_core::git::git_common_dir(&checkout_root).ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "resolved checkout has no stable git common directory",
        )
    })?;
    let mut matches = projects
        .iter()
        .filter_map(|project| {
            let project_root = canonical_directory(Path::new(&project.canonical_path)).ok()?;
            let project_git_root = bbox_corpus_core::git::git_root_for_path(&project_root)?;
            if bbox_corpus_core::git::git_common_dir(&project_git_root).as_ref() != Some(&common) {
                return None;
            }
            let relpath = project_root.strip_prefix(&project_git_root).ok()?;
            if !raw_rel.starts_with(relpath) {
                return None;
            }
            Some((relpath.components().count(), project))
        })
        .collect::<Vec<_>>();
    matches.sort_by_key(|(depth, _)| *depth);
    let (depth, selected) = matches.pop().ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::AttachmentNotFound,
            "resolved checkout contains no registered project scope",
        )
    })?;
    if matches
        .last()
        .is_some_and(|(other_depth, _)| *other_depth == depth)
    {
        return Err(access_error(
            CheckoutAccessErrorCode::SelectorMismatch,
            "legacy path resolves to more than one project scope",
        ));
    }
    Ok(selected)
}

fn current_branch_ref(checkout_root: &Path) -> Option<String> {
    bbox_corpus_core::git::current_branch(checkout_root)
        .map(|branch| format!("refs/heads/{branch}"))
}

fn unique_project<'a>(
    project_id: &str,
    projects: &'a [ProjectRecord],
) -> std::result::Result<&'a ProjectRecord, CheckoutAccessError> {
    let mut matching = projects
        .iter()
        .filter(|project| project.project_id == project_id);
    let project = matching.next().ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::AttachmentNotFound,
            "project id is not registered",
        )
    })?;
    if matching.next().is_some() {
        return Err(access_error(
            CheckoutAccessErrorCode::SelectorMismatch,
            "project id is ambiguous in the version-1 registry",
        ));
    }
    Ok(project)
}

fn project_scope(project: &ProjectRecord) -> Option<PublishedScope> {
    crate::publisher::project_published_scope(project, bbox_config::config::read_repo_id_inputs)
}

struct ResolvedRoots {
    checkout_root: PathBuf,
    project_root: PathBuf,
}

fn resolution_roots(
    project_id: &str,
    requested_project_root: &Path,
    projects: &[ProjectRecord],
    intent: ResolveIntent,
) -> std::result::Result<ResolvedRoots, CheckoutAccessError> {
    let raw = requested_project_root.to_str().ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "project root is not valid UTF-8 for resolver validation",
        )
    })?;
    let context = resolve_project_context(raw, projects, intent).ok_or_else(|| {
        access_error(
            CheckoutAccessErrorCode::ConservativePathGateDenied,
            "project resolver denied checkout access",
        )
    })?;
    if context.project_id != project_id {
        return Err(access_error(
            CheckoutAccessErrorCode::ProjectMismatch,
            "project resolver selected a different project",
        ));
    }
    let project = unique_project(project_id, projects)?;
    let base_project_root = canonical_directory(Path::new(&project.canonical_path))?;
    let base_checkout_root = bbox_corpus_core::git::git_root_for_path(&base_project_root)
        .unwrap_or_else(|| base_project_root.clone());
    let base_checkout_root = canonical_directory(&base_checkout_root)?;
    let relative_project_root = base_project_root
        .strip_prefix(&base_checkout_root)
        .map_err(|_| {
            access_error(
                CheckoutAccessErrorCode::ConservativePathGateDenied,
                "registered project root is outside its checkout root",
            )
        })?;

    let checkout_root = context
        .checkout
        .as_ref()
        .map(|checkout| canonical_directory(Path::new(&checkout.checkout_dir)))
        .transpose()?
        .unwrap_or(base_checkout_root);
    let project_root = if relative_project_root.as_os_str().is_empty() {
        checkout_root.clone()
    } else {
        canonical_directory(&checkout_root.join(relative_project_root))?
    };
    Ok(ResolvedRoots {
        checkout_root,
        project_root,
    })
}

fn join_scope_relpath(
    checkout_root: &Path,
    relpath: &str,
) -> std::result::Result<PathBuf, CheckoutAccessError> {
    if relpath == "." {
        return Ok(checkout_root.to_path_buf());
    }
    let relpath = Path::new(relpath);
    if relpath.as_os_str().is_empty()
        || relpath.is_absolute()
        || relpath
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(access_error(
            CheckoutAccessErrorCode::ScopeMismatch,
            "published scope contains an unsafe project relpath",
        ));
    }
    Ok(checkout_root.join(relpath))
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

fn deterministic_id(prefix: &str, components: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"blackbox-checkout-access-v1\0");
    digest.update(prefix.as_bytes());
    for component in components {
        digest.update(b"\0");
        digest.update(component.as_bytes());
    }
    let encoded = hex::encode(digest.finalize());
    format!("{prefix}-{}", &encoded[..32])
}

fn bounded_non_path_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 256
        && !value.contains('/')
        && !value.contains('\\')
        && !value.chars().any(char::is_whitespace)
}

fn access_error(code: CheckoutAccessErrorCode, diagnostic: &str) -> CheckoutAccessError {
    CheckoutAccessError::new(code, diagnostic)
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use crate::checkout_access::{
        CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessObservations,
        CheckoutAccessRequest, CheckoutAccessSourceLane, CheckoutAttachmentSelector,
    };

    use super::*;

    type ProjectStore = Arc<RwLock<ProjectRegistry>>;
    type CheckoutStore = Arc<RwLock<CheckoutRegistry>>;

    fn stores(root: &Path) -> (ProjectStore, CheckoutStore) {
        let projects = Arc::new(RwLock::new(
            ProjectRegistry::open(root.join("projects.json")).unwrap(),
        ));
        let checkouts = Arc::new(RwLock::new(
            CheckoutRegistry::open(&root.join("checkout-registry.json")).unwrap(),
        ));
        (projects, checkouts)
    }

    fn broker(projects: ProjectStore, checkouts: CheckoutStore) -> CheckoutAccessBroker {
        CheckoutAccessBroker::new(
            Arc::new(V1CheckoutAccessAuthority::new(projects, checkouts)),
            CheckoutAccessObservations::in_memory(),
        )
    }

    fn request(
        project_id: &str,
        attachment: CheckoutAttachmentSelector,
        scope: Option<PublishedScope>,
        kind: CheckoutAccessKind,
        intent: CheckoutAccessIntent,
    ) -> CheckoutAccessRequest {
        let source_lane = match &attachment {
            CheckoutAttachmentSelector::Selected => CheckoutAccessSourceLane::LegacyProjectRecord,
            CheckoutAttachmentSelector::CheckoutId(_) => {
                CheckoutAccessSourceLane::LegacyCheckoutRegistry
            }
            CheckoutAttachmentSelector::AttachmentId(_) => {
                CheckoutAccessSourceLane::NativeAttachment
            }
            CheckoutAttachmentSelector::LegacyPath(_) => {
                CheckoutAccessSourceLane::LegacyPathResolver
            }
        };
        CheckoutAccessRequest {
            project_id: project_id.into(),
            attachment,
            expected_scope: scope,
            kind,
            intent,
            source_lane,
        }
    }

    #[test]
    fn selected_base_project_grants_read_and_existing_write_gate() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let (projects, checkouts) = stores(&root);
        let record = projects.write().register_path(&project).unwrap();

        let read = broker(projects.clone(), checkouts.clone())
            .acquire(request(
                &record.project_id,
                CheckoutAttachmentSelector::Selected,
                None,
                CheckoutAccessKind::LocalProjectWalk,
                CheckoutAccessIntent::Read,
            ))
            .unwrap();
        assert_eq!(read.project_root(), project.canonicalize().unwrap());
        assert!(read.attachment_id().starts_with("v1-attachment-"));
        assert!(bounded_non_path_id(read.attachment_id()));

        broker(projects, checkouts)
            .acquire(request(
                &record.project_id,
                CheckoutAttachmentSelector::Selected,
                None,
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .expect("exact registered base passes the existing write gate");
    }

    #[test]
    fn managed_checkout_selection_grants_read_and_write() {
        let fixture = GitFixture::new("bro-fleet/managed");
        let read = fixture
            .broker()
            .acquire(fixture.request(CheckoutAccessKind::GitHistory, CheckoutAccessIntent::Read))
            .unwrap();
        assert_eq!(read.checkout_id(), fixture.checkout_id);
        assert_eq!(read.checkout_root(), fixture.checkout_root);
        assert!(bounded_non_path_id(read.attachment_id()));
        assert_eq!(
            read.attachment_id(),
            fixture
                .broker()
                .acquire(
                    fixture.request(CheckoutAccessKind::GitHistory, CheckoutAccessIntent::Read,)
                )
                .unwrap()
                .attachment_id()
        );

        fixture
            .broker()
            .acquire(fixture.request(
                CheckoutAccessKind::RenderFileProvider,
                CheckoutAccessIntent::Write,
            ))
            .expect("managed worktree grants render's dedicated write capability");
    }

    #[test]
    fn arbitrary_checkout_is_readable_but_write_gate_denies() {
        let fixture = GitFixture::new("arc/unmanaged");
        fixture
            .broker()
            .acquire(fixture.request(CheckoutAccessKind::GitHistory, CheckoutAccessIntent::Read))
            .expect("read resolver admits an arbitrary worktree");

        let error = fixture
            .broker()
            .acquire(fixture.request(
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap_err();
        assert_eq!(error.code, CheckoutAccessErrorCode::CapabilityDenied);
    }

    #[test]
    fn legacy_path_write_resolves_managed_checkout_inside_authority() {
        let fixture = GitFixture::new("bro-fleet/legacy-path");
        std::fs::remove_file(fixture.checkout_root.join(".bbox/local/checkout-id")).unwrap();
        let lease = fixture
            .broker()
            .acquire(request(
                "",
                CheckoutAttachmentSelector::LegacyPath(
                    fixture.checkout_root.to_string_lossy().into_owned(),
                ),
                None,
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap();
        assert_eq!(lease.project_id(), fixture.record.project_id);
        assert_eq!(lease.checkout_root(), fixture.checkout_root);
        assert!(
            lease
                .branch_ref()
                .is_some_and(|value| value.ends_with("legacy-path"))
        );
        assert_eq!(
            read_checkout_id(&fixture.checkout_root.join(".bbox/local/checkout-id"))
                .unwrap()
                .as_deref(),
            Some(lease.checkout_id())
        );
    }

    #[test]
    fn legacy_path_write_maps_plain_subdirectory_to_registered_base() {
        let fixture = GitFixture::new("bro-fleet/plain-subdir-fixture");
        let base = PathBuf::from(&fixture.record.canonical_path);
        let subdir = base.join("src");
        std::fs::create_dir(&subdir).unwrap();
        let lease = fixture
            .broker()
            .acquire(request(
                "",
                CheckoutAttachmentSelector::LegacyPath(subdir.to_string_lossy().into_owned()),
                None,
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap();
        assert_eq!(lease.project_root(), base);
        assert_eq!(lease.checkout_root(), base);
    }

    #[test]
    fn legacy_path_write_rejects_unmanaged_checkout() {
        let fixture = GitFixture::new("arc/unmanaged-legacy-path");
        let error = fixture
            .broker()
            .acquire(request(
                "",
                CheckoutAttachmentSelector::LegacyPath(
                    fixture.checkout_root.to_string_lossy().into_owned(),
                ),
                None,
                CheckoutAccessKind::RepositoryMutation,
                CheckoutAccessIntent::Write,
            ))
            .unwrap_err();
        assert_eq!(error.code, CheckoutAccessErrorCode::AttachmentNotFound);
    }

    #[test]
    fn checkout_selection_rejects_unknown_scope_inactive_and_ambiguous_rows() {
        let fixture = GitFixture::new("bro-fleet/selection-errors");
        let unknown = fixture
            .broker()
            .acquire(request(
                &fixture.record.project_id,
                CheckoutAttachmentSelector::CheckoutId("unknown-checkout".into()),
                Some(fixture.scope.clone()),
                CheckoutAccessKind::GitHistory,
                CheckoutAccessIntent::Read,
            ))
            .unwrap_err();
        assert_eq!(unknown.code, CheckoutAccessErrorCode::AttachmentNotFound);

        let wrong_scope = fixture
            .broker()
            .acquire(request(
                &fixture.record.project_id,
                CheckoutAttachmentSelector::CheckoutId(fixture.checkout_id.clone()),
                Some(PublishedScope {
                    repo_id: "different-family".into(),
                    bbox_root_relpath: ".".into(),
                }),
                CheckoutAccessKind::GitHistory,
                CheckoutAccessIntent::Read,
            ))
            .unwrap_err();
        assert_eq!(wrong_scope.code, CheckoutAccessErrorCode::ScopeMismatch);

        let marker = fixture.checkout_root.join(".bbox/local/checkout-id");
        std::fs::write(&marker, "replacement-checkout\n").unwrap();
        let replaced_identity = fixture
            .broker()
            .acquire(fixture.request(CheckoutAccessKind::GitHistory, CheckoutAccessIntent::Read))
            .unwrap_err();
        assert_eq!(
            replaced_identity.code,
            CheckoutAccessErrorCode::CheckoutIdentityMismatch
        );
        std::fs::write(&marker, format!("{}\n", fixture.checkout_id)).unwrap();

        fixture
            .checkouts
            .write()
            .register(CheckoutRow {
                project_id: None,
                checkout_id: "inactive-checkout".into(),
                checkout_dir: fixture
                    .checkout_root
                    .join("missing")
                    .to_string_lossy()
                    .into(),
                repo_id: Some(fixture.scope.repo_id.clone()),
                bbox_root_relpath: Some(fixture.scope.bbox_root_relpath.clone()),
                branch_ref: None,
            })
            .unwrap();
        let inactive = fixture
            .broker()
            .acquire(request(
                &fixture.record.project_id,
                CheckoutAttachmentSelector::CheckoutId("inactive-checkout".into()),
                Some(fixture.scope.clone()),
                CheckoutAccessKind::GitHistory,
                CheckoutAccessIntent::Read,
            ))
            .unwrap_err();
        assert_eq!(inactive.code, CheckoutAccessErrorCode::AttachmentInactive);

        let duplicate = CheckoutRow {
            project_id: None,
            checkout_id: fixture.checkout_id.clone(),
            checkout_dir: fixture.checkout_root.to_string_lossy().into(),
            repo_id: Some(fixture.scope.repo_id.clone()),
            bbox_root_relpath: Some(fixture.scope.bbox_root_relpath.clone()),
            branch_ref: None,
        };
        std::fs::write(
            &fixture.registry_path,
            serde_json::to_vec(&serde_json::json!({
                "version": 2,
                "checkouts": [duplicate.clone(), duplicate]
            }))
            .unwrap(),
        )
        .unwrap();
        *fixture.checkouts.write() = CheckoutRegistry::open(&fixture.registry_path).unwrap();
        let ambiguous = fixture
            .broker()
            .acquire(fixture.request(CheckoutAccessKind::GitHistory, CheckoutAccessIntent::Read))
            .unwrap_err();
        assert_eq!(ambiguous.code, CheckoutAccessErrorCode::SelectorMismatch);
    }

    struct GitFixture {
        _tmp: tempfile::TempDir,
        projects: ProjectStore,
        checkouts: CheckoutStore,
        registry_path: PathBuf,
        record: ProjectRecord,
        scope: PublishedScope,
        checkout_root: PathBuf,
        checkout_id: String,
    }

    impl GitFixture {
        fn new(branch: &str) -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let root = tmp.path().canonicalize().unwrap();
            let repo = root.join("repo");
            init_git_repo(&repo);
            let (projects, checkouts) = stores(&root);
            let record = projects.write().register_path(&repo).unwrap();
            let checkout = root.join("checkout");
            add_worktree(&repo, branch, &checkout);
            let checkout_root = checkout.canonicalize().unwrap();
            let checkout_id =
                bbox_corpus_core::identity::ensure_checkout_id(&checkout_root).unwrap();
            let scope = PublishedScope {
                repo_id: "repo-family".into(),
                bbox_root_relpath: ".".into(),
            };
            checkouts
                .write()
                .register(CheckoutRow {
                    project_id: None,
                    checkout_id: checkout_id.clone(),
                    checkout_dir: checkout_root.to_string_lossy().into(),
                    repo_id: Some(scope.repo_id.clone()),
                    bbox_root_relpath: Some(scope.bbox_root_relpath.clone()),
                    branch_ref: Some(format!("refs/heads/{branch}")),
                })
                .unwrap();
            Self {
                _tmp: tmp,
                projects,
                checkouts,
                registry_path: root.join("checkout-registry.json"),
                record,
                scope,
                checkout_root,
                checkout_id,
            }
        }

        fn broker(&self) -> CheckoutAccessBroker {
            broker(self.projects.clone(), self.checkouts.clone())
        }

        fn request(
            &self,
            kind: CheckoutAccessKind,
            intent: CheckoutAccessIntent,
        ) -> CheckoutAccessRequest {
            request(
                &self.record.project_id,
                CheckoutAttachmentSelector::CheckoutId(self.checkout_id.clone()),
                Some(self.scope.clone()),
                kind,
                intent,
            )
        }
    }

    fn init_git_repo(path: &Path) {
        std::fs::create_dir_all(path.join(".bbox")).unwrap();
        git(path, &["init"]);
        git(path, &["config", "user.email", "test@example.com"]);
        git(path, &["config", "user.name", "Test"]);
        git(path, &["config", "commit.gpgsign", "false"]);
        std::fs::write(
            path.join(".bbox/config.toml"),
            "[project]\nrepo_id = \"repo-family\"\n",
        )
        .unwrap();
        std::fs::write(path.join("README.md"), "fixture\n").unwrap();
        git(path, &["add", "."]);
        git(path, &["commit", "-m", "initial"]);
    }

    fn add_worktree(repo: &Path, branch: &str, destination: &Path) {
        git(
            repo,
            &[
                "worktree",
                "add",
                "-b",
                branch,
                destination.to_str().unwrap(),
            ],
        );
    }

    fn git(cwd: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderCheckoutSelection, ProviderContext, ProviderProjectAuthority,
    empty_neighborhood_view, ensure_type, schema, truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::{
    AttachmentKind, AttachmentStatus, ProjectId, ProjectScope,
};
use bbox_corpus_core::project_selector::{
    ProjectSelectorRequest, ResolveIntent, ResolvedAttachment, ResolvedProjectIdentity,
    SelectorClass, SessionCheckoutRef,
};
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector, ValidatedCheckoutLease,
};
use bbox_indexing::checkout_registry::CheckoutRow;
use bbox_indexing::project_catalog_store::{ProjectCatalogState, ProjectCatalogStore};
use bbox_indexing::project_resolver::ProjectResolverEngine;
use bbox_indexing::projects::ProjectRecord;

pub struct FileProvider;

#[derive(Debug, Clone)]
pub struct ResolvedFile {
    pub project_id: String,
    pub project_root: PathBuf,
    pub file_path: PathBuf,
    pub relative_path: String,
    pub content: Vec<u8>,
}

impl InspectableEntityProvider for FileProvider {
    fn entity_type(&self) -> EntityType {
        EntityType::File
    }

    fn owns_ref(&self, r: &EntityRef) -> bool {
        matches!(r, EntityRef::File { .. })
    }

    fn get_entity(&self, ctx: &ProviderContext<'_>, r: &EntityRef) -> Result<EntityView> {
        ensure_type(r, self.entity_type())?;
        let EntityRef::File { path } = r else {
            unreachable!();
        };
        let mut properties = BTreeMap::new();
        properties.insert("path".into(), path.clone());
        if ctx.stores().is_some() {
            let resolved = resolve_file(ctx, path)?;
            properties.insert("project_id".into(), resolved.project_id);
            properties.insert(
                "project_root".into(),
                resolved.project_root.to_string_lossy().into_owned(),
            );
            properties.insert(
                "file_path".into(),
                resolved.file_path.to_string_lossy().into_owned(),
            );
            properties.insert("relative_path".into(), resolved.relative_path);
            properties.insert("bytes".into(), resolved.content.len().to_string());
            properties.insert("content_preview".into(), preview(&resolved.content));
        }
        Ok(empty_neighborhood_view(r, properties))
    }

    fn schema(&self) -> EntitySchemaView {
        schema(
            self.entity_type(),
            &[
                "path",
                "project_id",
                "project_root",
                "file_path",
                "relative_path",
                "bytes",
                "content_preview",
            ],
            &["IN_PROJECT"],
            &["path", "project_id", "relative_path"],
        )
    }

    fn expected_edge_families(&self, _r: &EntityRef) -> Vec<EdgeFamilyExpectation> {
        Vec::new()
    }

    fn recommended_next_hops(
        &self,
        _entity: &EntityView,
        _full_neighborhood: &Neighborhood,
    ) -> Vec<NextHop> {
        Vec::new()
    }

    fn compact_label(&self, _ctx: &ProviderContext<'_>, r: &EntityRef) -> Option<String> {
        let EntityRef::File { path } = r else {
            return None;
        };
        Some(truncate_label(path))
    }
}

pub fn resolve_file(ctx: &ProviderContext<'_>, path: &str) -> Result<ResolvedFile> {
    let stores = ctx
        .stores()
        .ok_or_else(|| anyhow!("file refs require a registered project context"))?;
    if let ProviderProjectAuthority::Catalog { catalog } = stores.project_authority {
        return resolve_catalog_file(
            catalog,
            path,
            ctx.checkout_selection(),
            stores.checkout_access,
        );
    }
    let snapshot = stores.projects.records_snapshot();
    let projects = snapshot.records.clone();
    if projects.is_empty() {
        return Err(no_attached_projects_error(
            snapshot.corpus_project_ids.len(),
        ));
    }

    let raw = Path::new(path);
    if raw.is_absolute() {
        let rows = {
            let registry = stores.checkout_registry.read();
            registry.rows().to_vec()
        };
        resolve_absolute(raw, &projects, &rows, stores.checkout_access)
    } else {
        resolve_relative(
            raw,
            &projects,
            ctx.checkout_selection(),
            stores.checkout_access,
        )
    }
}

/// One selected catalog attachment plus the scope its project records.
struct CatalogAttachmentTarget {
    attachment_id: String,
    expected_scope: Option<PublishedScope>,
}

fn resolve_error(error: bbox_corpus_core::project_selector::ProjectResolveError) -> anyhow::Error {
    anyhow!("{error}")
}

fn published_scope_of(project: &ResolvedProjectIdentity) -> Option<PublishedScope> {
    match project {
        ResolvedProjectIdentity::Catalog { project } => match &project.scope {
            ProjectScope::Published(scope) => Some(scope.clone()),
            ProjectScope::LegacyLocal => None,
        },
        ResolvedProjectIdentity::V1Compat { .. } => None,
    }
}

fn unique_active_base(
    state: &ProjectCatalogState,
    project_id: &str,
) -> Option<CatalogAttachmentTarget> {
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

/// Select one attachment through the SHARED resolver: explicit attachment,
/// session checkout, operator default, single active attachment. D-033 item 3
/// fixes the unique active `Base` as the final rung and the resolver does not
/// implement it, so it is applied here and only where the resolver reported
/// ambiguity; a project whose default or sole attachment already resolved is
/// never redirected to its base.
fn catalog_attachment_target(
    state: &ProjectCatalogState,
    project_id: &str,
    selection: Option<&ProviderCheckoutSelection>,
) -> Result<CatalogAttachmentTarget> {
    let engine = ProjectResolverEngine::v2(state.catalog(), state.attachments());
    let request = ProjectSelectorRequest {
        selector: Some(project_id.to_owned()),
        session: selection.map(|selection| SessionCheckoutRef {
            checkout_id: Some(selection.checkout_id.clone()),
            checkout_project_dir: None,
        }),
        intent: ResolveIntent::Read,
        class: SelectorClass::Selection,
        ..Default::default()
    };
    let resolved = match engine.resolve_attached(&request) {
        Ok(resolved) => resolved,
        Err(error) if error.code() == "error.project_attachment_ambiguous" => {
            return unique_active_base(state, project_id).ok_or_else(|| resolve_error(error));
        }
        Err(error) => return Err(resolve_error(error)),
    };
    let ResolvedAttachment::Catalog { attachment_id, .. } = &resolved.attachment else {
        bail!("error.project_attachment_required: {project_id}");
    };
    Ok(CatalogAttachmentTarget {
        attachment_id: attachment_id.clone(),
        expected_scope: published_scope_of(&resolved.project),
    })
}

/// The catalog project a relative ref acts on when the session named none:
/// exactly one project carrying an active attachment. Zero and many are typed
/// refusals; nothing silently picks a project.
fn sole_attached_project(state: &ProjectCatalogState) -> Result<String> {
    let mut attached = state
        .attachments()
        .attachments
        .values()
        .filter(|row| row.status == AttachmentStatus::Attached)
        .map(|row| row.project_id.as_str().to_string())
        .collect::<Vec<_>>();
    attached.sort();
    attached.dedup();
    match attached.as_slice() {
        [project_id] => Ok(project_id.clone()),
        [] => bail!(
            "error.project_attachment_required: no catalog project has an active attachment on this host"
        ),
        _ => bail!(
            "error.project_selector_ambiguous: relative file refs require a session checkout when more than one project is attached"
        ),
    }
}

/// Catalog resolution for a `file:` ref.
///
/// Absolute matching is limited to ACTIVE attachment metadata: the path rides
/// the catalog resolver's own containment arms through the broker, and the
/// project-relative path is stripped from the acquired lease root. No
/// `ProjectRecord::canonical_path` is read, which is what lets a project whose
/// only attachment is a worktree (and which therefore has no compatibility
/// row at all) resolve correctly.
fn resolve_catalog_file(
    catalog: &ProjectCatalogStore,
    path: &str,
    selection: Option<&ProviderCheckoutSelection>,
    broker: &CheckoutAccessBroker,
) -> Result<ResolvedFile> {
    let raw = Path::new(path);
    if raw.is_absolute() {
        if raw
            .components()
            .any(|component| matches!(component, Component::ParentDir))
        {
            bail!("absolute file ref may not contain parent traversal");
        }
        let lease = broker
            .acquire(CheckoutAccessRequest {
                project_id: String::new(),
                attachment: CheckoutAttachmentSelector::LegacyPath(path.to_owned()),
                expected_scope: None,
                kind: CheckoutAccessKind::RenderFileProvider,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
            })
            .map_err(checkout_access_error)?;
        let relative = raw
            .strip_prefix(lease.project_root())
            .map_err(|_| anyhow!("file ref is outside every active checkout attachment"))?
            .to_path_buf();
        if relative.as_os_str().is_empty() {
            bail!("file ref must name a file inside the attachment");
        }
        let project_id = lease.project_id().to_owned();
        return read_with_lease(broker, project_id, lease, &relative);
    }

    if raw.as_os_str().is_empty()
        || raw.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("file ref must be a non-empty relative path without parent traversal");
    }
    let state = catalog.snapshot().map_err(anyhow::Error::new)?;
    let project_id = match selection {
        Some(selection) => selection.project_id.clone(),
        None => sole_attached_project(&state)?,
    };
    let target = catalog_attachment_target(&state, &project_id, selection)?;
    let lease = broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.clone(),
            attachment: CheckoutAttachmentSelector::AttachmentId(target.attachment_id),
            expected_scope: target.expected_scope,
            kind: CheckoutAccessKind::RenderFileProvider,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::NativeAttachment,
        })
        .map_err(checkout_access_error)?;
    read_with_lease(broker, project_id, lease, raw)
}

/// A `file:` ref genuinely needs a checkout, so attachment-binding is
/// correct here; the MESSAGE was what misled. In a catalog-mode deployment
/// whose projects are all remote-only, "no registered project" sends the
/// reader looking for a registration that already exists (Phase 3 plan
/// section 7 item 3). Distinguish the two states.
fn no_attached_projects_error(registered_projects: usize) -> anyhow::Error {
    if registered_projects == 0 {
        anyhow!("file refs require at least one registered project")
    } else {
        anyhow!(
            "file refs require a project with an attached checkout; \
             {registered_projects} registered project(s) have no attachment on this host"
        )
    }
}

fn checkout_access_error(
    error: bbox_indexing::checkout_access::CheckoutAccessError,
) -> anyhow::Error {
    anyhow!(
        "error.checkout_access.{}: {}",
        error.code.as_str(),
        error.diagnostic
    )
}

fn discover_scope(
    broker: &CheckoutAccessBroker,
    project_id: &str,
) -> Result<Option<PublishedScope>> {
    let lease = broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_string(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(checkout_access_error)?;
    let scope = lease.published_scope().cloned();
    broker.revalidate(&lease).map_err(checkout_access_error)?;
    Ok(scope)
}

fn selected_lease(
    broker: &CheckoutAccessBroker,
    project: &ProjectRecord,
) -> Result<ValidatedCheckoutLease> {
    let expected_scope = discover_scope(broker, &project.project_id)?;
    broker
        .acquire(CheckoutAccessRequest {
            project_id: project.project_id.clone(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope,
            kind: CheckoutAccessKind::RenderFileProvider,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(checkout_access_error)
}

fn checkout_lease(
    broker: &CheckoutAccessBroker,
    selection: &ProviderCheckoutSelection,
) -> Result<ValidatedCheckoutLease> {
    broker
        .acquire(CheckoutAccessRequest {
            project_id: selection.project_id.clone(),
            attachment: CheckoutAttachmentSelector::CheckoutId(selection.checkout_id.clone()),
            expected_scope: Some(selection.published_scope.clone()),
            kind: CheckoutAccessKind::RenderFileProvider,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyCheckoutRegistry,
        })
        .map_err(checkout_access_error)
}

fn read_with_lease(
    broker: &CheckoutAccessBroker,
    project_id: String,
    lease: ValidatedCheckoutLease,
    relative: &Path,
) -> Result<ResolvedFile> {
    let read = lease
        .read_relative_file(relative)
        .map_err(checkout_access_error);
    broker.revalidate(&lease).map_err(checkout_access_error)?;
    let (file_path, content) = read?;
    Ok(ResolvedFile {
        project_id,
        project_root: lease.project_root().to_path_buf(),
        relative_path: relative.to_string_lossy().into_owned(),
        file_path,
        content,
    })
}

fn resolve_relative(
    raw: &Path,
    projects: &[ProjectRecord],
    selection: Option<&ProviderCheckoutSelection>,
    broker: &CheckoutAccessBroker,
) -> Result<ResolvedFile> {
    if raw.as_os_str().is_empty()
        || raw.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        bail!("file ref must be a non-empty relative path without parent traversal");
    }
    let (project_id, lease) = if let Some(selection) = selection {
        let matches = projects
            .iter()
            .filter(|project| project.project_id == selection.project_id)
            .count();
        if matches != 1 {
            bail!("error.project_mismatch: session checkout project is not uniquely registered");
        }
        (
            selection.project_id.clone(),
            checkout_lease(broker, selection)?,
        )
    } else {
        let [project] = projects else {
            bail!(
                "error.project_ambiguous: relative file refs require a session checkout or exactly one registered project"
            );
        };
        (project.project_id.clone(), selected_lease(broker, project)?)
    };
    read_with_lease(broker, project_id, lease, raw)
}

#[derive(Clone)]
enum AbsoluteSelection {
    Selected {
        project: ProjectRecord,
        relative: PathBuf,
    },
    Checkout {
        project: ProjectRecord,
        row: CheckoutRow,
        scope: PublishedScope,
        relative: PathBuf,
    },
}

fn scope_root(checkout_root: &Path, scope: &PublishedScope) -> Result<PathBuf> {
    if scope.bbox_root_relpath() == "." {
        return Ok(checkout_root.to_path_buf());
    }
    let relative = Path::new(scope.bbox_root_relpath());
    if relative.as_os_str().is_empty()
        || relative.is_absolute()
        || relative
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("checkout scope has an unsafe relative project root");
    }
    Ok(checkout_root.join(relative))
}

fn resolve_absolute(
    raw: &Path,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
    broker: &CheckoutAccessBroker,
) -> Result<ResolvedFile> {
    if raw
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        bail!("absolute file ref may not contain parent traversal");
    }

    let scopes = projects
        .iter()
        .map(|project| {
            discover_scope(broker, &project.project_id)
                .map(|scope| scope.map(|scope| (project, scope)))
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    let mut candidates = Vec::<(usize, bool, AbsoluteSelection)>::new();
    for project in projects {
        let root = Path::new(&project.canonical_path);
        if let Ok(relative) = raw.strip_prefix(root)
            && !relative.as_os_str().is_empty()
        {
            candidates.push((
                root.components().count(),
                false,
                AbsoluteSelection::Selected {
                    project: project.clone(),
                    relative: relative.to_path_buf(),
                },
            ));
        }
    }
    for row in rows {
        let Some(scope) = row.published_scope() else {
            continue;
        };
        let root = scope_root(Path::new(&row.checkout_dir), &scope)?;
        let Ok(relative) = raw.strip_prefix(&root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        for (project, candidate_scope) in &scopes {
            if *candidate_scope == scope {
                candidates.push((
                    root.components().count(),
                    true,
                    AbsoluteSelection::Checkout {
                        project: (*project).clone(),
                        row: row.clone(),
                        scope: scope.clone(),
                        relative: relative.to_path_buf(),
                    },
                ));
            }
        }
    }
    let deepest = candidates
        .iter()
        .map(|(depth, _, _)| *depth)
        .max()
        .ok_or_else(|| anyhow!("file ref is outside every registered checkout attachment"))?;
    candidates.retain(|(depth, _, _)| *depth == deepest);
    if candidates.iter().any(|(_, checkout, _)| *checkout) {
        candidates.retain(|(_, checkout, _)| *checkout);
    }
    if candidates.len() != 1 {
        bail!("file ref is ambiguous across registered checkout attachments");
    }
    match candidates.pop().expect("one absolute file candidate").2 {
        AbsoluteSelection::Selected { project, relative } => {
            let lease = selected_lease(broker, &project)?;
            read_with_lease(broker, project.project_id, lease, &relative)
        }
        AbsoluteSelection::Checkout {
            project,
            row,
            scope,
            relative,
        } => {
            let lease = broker
                .acquire(CheckoutAccessRequest {
                    project_id: project.project_id.clone(),
                    attachment: CheckoutAttachmentSelector::CheckoutId(row.checkout_id),
                    expected_scope: Some(scope),
                    kind: CheckoutAccessKind::RenderFileProvider,
                    intent: CheckoutAccessIntent::Read,
                    source_lane: CheckoutAccessSourceLane::LegacyCheckoutRegistry,
                })
                .map_err(checkout_access_error)?;
            read_with_lease(broker, project.project_id, lease, &relative)
        }
    }
}

fn preview(content: &[u8]) -> String {
    let text = String::from_utf8_lossy(content);
    text.chars().take(400).collect()
}

// Test-scope fixture I/O on the test thread, not a tokio worker: the
// concurrency invariant this lint enforces (I2) is about runtime workers, and
// clippy.toml names test scopes as allowed. bbox-providers has no crate-root
// allowance, so the sanctioned scope is spelled here.
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod tests {
    use super::*;
    use bbox_indexing::checkout_access::{
        CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError,
        CheckoutAccessErrorCode, CheckoutAccessObservations, CheckoutAttachmentStatus,
        DenyCheckoutAccess,
    };
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FileAuthority {
        base_root: PathBuf,
        checkout_root: PathBuf,
        resolves: AtomicUsize,
    }

    impl CheckoutAccessAuthority for FileAuthority {
        fn resolve(
            &self,
            request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            let (attachment_id, checkout_id, project_root) = match &request.attachment {
                CheckoutAttachmentSelector::Selected => (
                    "attachment-base".to_string(),
                    "checkout-base".to_string(),
                    self.base_root.clone(),
                ),
                CheckoutAttachmentSelector::CheckoutId(checkout_id) => (
                    "attachment-session".to_string(),
                    checkout_id.clone(),
                    self.checkout_root.clone(),
                ),
                CheckoutAttachmentSelector::AttachmentId(_) => {
                    return Err(CheckoutAccessError::new(
                        CheckoutAccessErrorCode::AttachmentNotFound,
                        "test authority has no native attachment",
                    ));
                }
                CheckoutAttachmentSelector::LegacyPath(_) => {
                    return Err(CheckoutAccessError::new(
                        CheckoutAccessErrorCode::AttachmentNotFound,
                        "test authority has no legacy path selector",
                    ));
                }
            };
            Ok(CheckoutAccessCandidate {
                project_id: request.project_id.clone(),
                attachment_id,
                checkout_id,
                published_scope: Some(scope()),
                branch_ref: Some("refs/heads/main".into()),
                checkout_root: project_root.clone(),
                project_root,
                status: CheckoutAttachmentStatus::Active,
                capabilities: BTreeSet::from([request.kind]),
                lifetime_guard: None,
            })
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            Ok(())
        }
    }

    struct CountingDeny {
        resolves: Arc<AtomicUsize>,
    }

    impl CheckoutAccessAuthority for CountingDeny {
        fn resolve(
            &self,
            _request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            Err(CheckoutAccessError::new(
                CheckoutAccessErrorCode::DeniedByTestProbe,
                "denied",
            ))
        }

        fn revalidate_conservative_path_gate(
            &self,
            _request: &CheckoutAccessRequest,
            _candidate: &CheckoutAccessCandidate,
        ) -> std::result::Result<(), CheckoutAccessError> {
            unreachable!()
        }
    }

    fn scope() -> PublishedScope {
        PublishedScope::try_new("repo-file-provider", ".").unwrap()
    }

    fn project(project_id: &str, root: &Path) -> ProjectRecord {
        ProjectRecord {
            project_id: project_id.into(),
            repo_id: None,
            canonical_path: root.to_string_lossy().into_owned(),
            registered_at: "2026-07-22T00:00:00Z".into(),
            is_git_repo: false,
            languages: BTreeSet::new(),
            aliases: BTreeSet::new(),
        }
    }

    #[test]
    fn empty_attached_projects_distinguishes_unregistered_from_unattached() {
        assert!(
            no_attached_projects_error(0)
                .to_string()
                .contains("at least one registered project")
        );
        let unattached = no_attached_projects_error(3).to_string();
        assert!(unattached.contains("attached checkout"), "{unattached}");
        assert!(
            unattached.contains("3 registered project(s)"),
            "{unattached}"
        );
    }

    #[test]
    fn relative_ref_uses_session_checkout_instead_of_selected_base() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let base = root.join("base");
        let checkout = root.join("checkout");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        std::fs::write(base.join("same.txt"), "base").unwrap();
        std::fs::write(checkout.join("same.txt"), "checkout").unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(FileAuthority {
                base_root: base.clone(),
                checkout_root: checkout,
                resolves: AtomicUsize::new(0),
            }),
            CheckoutAccessObservations::in_memory(),
        );
        let selection = ProviderCheckoutSelection {
            project_id: "project-1".into(),
            checkout_id: "checkout-session".into(),
            published_scope: scope(),
        };

        let resolved = resolve_relative(
            Path::new("same.txt"),
            &[project("project-1", &base)],
            Some(&selection),
            &broker,
        )
        .unwrap();

        assert_eq!(resolved.content, b"checkout");
        assert_eq!(resolved.project_id, "project-1");
    }

    #[test]
    fn ambiguous_relative_ref_fails_before_checkout_authority() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let resolves = Arc::new(AtomicUsize::new(0));
        let broker = CheckoutAccessBroker::new(
            Arc::new(CountingDeny {
                resolves: resolves.clone(),
            }),
            CheckoutAccessObservations::in_memory(),
        );

        let error = resolve_relative(
            Path::new("same.txt"),
            &[
                project("project-1", &root.join("one")),
                project("project-2", &root.join("two")),
            ],
            None,
            &broker,
        )
        .unwrap_err();

        assert!(error.to_string().starts_with("error.project_ambiguous:"));
        assert_eq!(resolves.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn checkout_denial_is_preserved_for_relative_and_absolute_refs() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::create_dir_all(&root).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );
        let projects = [project("project-1", &root)];

        let relative = resolve_relative(Path::new("file.txt"), &projects, None, &broker)
            .unwrap_err()
            .to_string();
        let absolute = resolve_absolute(&root.join("file.txt"), &projects, &[], &broker)
            .unwrap_err()
            .to_string();

        assert!(relative.starts_with("error.checkout_access.denied_by_test_probe:"));
        assert!(absolute.starts_with("error.checkout_access.denied_by_test_probe:"));
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_relative_read_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let base = root.join("base");
        let outside = root.join("outside");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        symlink(&outside, base.join("escape")).unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(FileAuthority {
                base_root: base.clone(),
                checkout_root: base.clone(),
                resolves: AtomicUsize::new(0),
            }),
            CheckoutAccessObservations::in_memory(),
        );

        let error = resolve_relative(
            Path::new("escape/secret.txt"),
            &[project("project-1", &base)],
            None,
            &broker,
        )
        .unwrap_err()
        .to_string();

        assert!(error.starts_with("error.checkout_access.conservative_path_gate_denied:"));
    }
}

/// Catalog-mode file-provider tests (Phase 5 plan section 13.5).
// Test-scope fixture I/O on the test thread, not a tokio worker: the
// concurrency invariant this lint enforces (I2) is about runtime workers, and
// clippy.toml names test scopes as allowed. bbox-providers has no crate-root
// allowance, so the sanctioned scope is spelled here.
#[allow(clippy::disallowed_methods)]
#[cfg(test)]
mod catalog_tests {
    use super::*;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, CheckoutAttachment, CorpusProject,
        LegacyPathBindingId, LegacyPathBindingStatus, LegacyPathLedgerEntry,
        LegacyPathRelationship,
    };
    use bbox_indexing::checkout_access::CheckoutAccessObservations;
    use std::sync::Arc;

    const PROJECT_ONE: &str = "p_000000000000000000000000000000a1";
    const PROJECT_TWO: &str = "p_000000000000000000000000000000b1";
    const ATTACHMENT_ONE: &str = "att_00000000000000000000000000000a01";
    const ATTACHMENT_TWO: &str = "att_00000000000000000000000000000a02";

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        store: Arc<ProjectCatalogStore>,
    }

    struct AttachSpec<'a> {
        project_id: &'a str,
        attachment_id: &'a str,
        dir_name: &'a str,
        kind: AttachmentKind,
        default_for_project: bool,
        render: bool,
    }

    fn spec<'a>(project_id: &'a str, attachment_id: &'a str, dir_name: &'a str) -> AttachSpec<'a> {
        AttachSpec {
            project_id,
            attachment_id,
            dir_name,
            kind: AttachmentKind::Base,
            default_for_project: false,
            render: true,
        }
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let catalog_root = root.join("catalog");
            std::fs::create_dir_all(&catalog_root).unwrap();
            let store = Arc::new(
                ProjectCatalogStore::initialize_empty(catalog_root.join("projects.json")).unwrap(),
            );
            Self {
                _directory: directory,
                root,
                store,
            }
        }

        fn scope(repo: &str) -> PublishedScope {
            PublishedScope::try_new(repo, ".").unwrap()
        }

        fn add_project(&self, project_id: &str, scope: &PublishedScope) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let scope = scope.clone();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |catalog, _attachments| {
                    catalog.projects.insert(
                        project_id.clone(),
                        CorpusProject {
                            project_id: project_id.clone(),
                            scope: ProjectScope::Published(scope.clone()),
                            operator_aliases: Default::default(),
                            nominated_aliases: Default::default(),
                            display_name: project_id.as_str().to_string(),
                            created_at: "2026-08-03T00:00:00Z".into(),
                            registered_at_compat: None,
                            repo_history: None,
                            languages: Default::default(),
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        fn attach(&self, spec: AttachSpec<'_>, scope: &PublishedScope) -> (PathBuf, String) {
            let checkout_dir = self.root.join(spec.dir_name);
            std::fs::create_dir_all(&checkout_dir).unwrap();
            let checkout_dir = checkout_dir.canonicalize().unwrap();
            let checkout_id =
                bbox_corpus_core::identity::ensure_checkout_id(&checkout_dir).unwrap();
            let project_id = ProjectId::parse(spec.project_id).unwrap();
            let attachment_id = AttachmentId::parse(spec.attachment_id).unwrap();
            let dir = checkout_dir.to_string_lossy().into_owned();
            let scope = scope.clone();
            let epoch = self.store.snapshot().unwrap().epoch();
            let id = checkout_id.clone();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    attachments.attachments.insert(
                        attachment_id.clone(),
                        CheckoutAttachment {
                            attachment_id: attachment_id.clone(),
                            project_id: project_id.clone(),
                            checkout_id: id.clone(),
                            checkout_dir: dir.clone(),
                            checkout_project_dir: dir.clone(),
                            project_root_relpath: ".".into(),
                            kind: spec.kind.clone(),
                            validated_scope: Some(scope.clone()),
                            computed_repo_hint: None,
                            branch_ref: Some("refs/heads/main".into()),
                            capabilities: AttachmentCapabilities {
                                render_output: spec.render,
                                ..Default::default()
                            },
                            status: AttachmentStatus::Attached,
                            attached_at: "2026-08-03T00:00:00Z".into(),
                            detached_at: None,
                        },
                    );
                    if spec.default_for_project {
                        attachments
                            .default_attachments
                            .insert(project_id.clone(), attachment_id.clone());
                    }
                    Ok(())
                })
                .unwrap();
            (checkout_dir, checkout_id)
        }

        /// Record a historical path in the legacy ledger. The ledger maps old
        /// paths to project ids for store-key compatibility; it grants no
        /// filesystem authority, and this fixture exists to prove that.
        fn record_legacy_path(&self, project_id: &str, historical: &Path) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let historical = historical.to_string_lossy().into_owned();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    let id =
                        LegacyPathBindingId::parse("lpb_11111111111111111111111111111111").unwrap();
                    attachments.legacy_path_bindings.insert(
                        id.clone(),
                        LegacyPathLedgerEntry {
                            legacy_path_binding_id: id,
                            historical_path: historical.clone(),
                            source_store: "knowledge".into(),
                            source_row_id: "row-1".into(),
                            inventory_epoch: 1,
                            status: LegacyPathBindingStatus::Mapped {
                                project_id: project_id.clone(),
                                relationship: LegacyPathRelationship::Root,
                            },
                        },
                    );
                    Ok(())
                })
                .unwrap();
        }

        fn broker(&self) -> CheckoutAccessBroker {
            CheckoutAccessBroker::new(
                Arc::new(
                    bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(
                        self.store.clone(),
                    ),
                ),
                CheckoutAccessObservations::in_memory(),
            )
        }

        fn resolve(
            &self,
            path: &str,
            selection: Option<&ProviderCheckoutSelection>,
        ) -> Result<ResolvedFile> {
            resolve_catalog_file(&self.store, path, selection, &self.broker())
        }
    }

    fn selection(
        project_id: &str,
        checkout_id: &str,
        scope: &PublishedScope,
    ) -> ProviderCheckoutSelection {
        ProviderCheckoutSelection {
            project_id: project_id.to_string(),
            checkout_id: checkout_id.to_string(),
            published_scope: scope.clone(),
        }
    }

    /// A session-pinned checkout selects its own attachment, not the host's
    /// default, and reads the session checkout's copy of the file.
    #[test]
    fn session_attachment_reads_its_own_checkout() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let mut base = spec(PROJECT_ONE, ATTACHMENT_ONE, "base");
        base.default_for_project = true;
        let (base_dir, _) = fixture.attach(base, &scope);
        let mut worktree = spec(PROJECT_ONE, ATTACHMENT_TWO, "worktree");
        worktree.kind = AttachmentKind::Worktree;
        let (worktree_dir, worktree_id) = fixture.attach(worktree, &scope);
        std::fs::write(base_dir.join("same.txt"), "base").unwrap();
        std::fs::write(worktree_dir.join("same.txt"), "worktree").unwrap();

        let resolved = fixture
            .resolve(
                "same.txt",
                Some(&selection(PROJECT_ONE, &worktree_id, &scope)),
            )
            .unwrap();

        assert_eq!(resolved.content, b"worktree");
        assert_eq!(resolved.project_id, PROJECT_ONE);
        assert_eq!(resolved.relative_path, "same.txt");
    }

    /// With no session pin, the operator-selected default decides.
    #[test]
    fn operator_default_decides_without_a_session() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let (base_dir, _) = fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "base"), &scope);
        let mut chosen = spec(PROJECT_ONE, ATTACHMENT_TWO, "chosen");
        chosen.kind = AttachmentKind::Worktree;
        chosen.default_for_project = true;
        let (chosen_dir, _) = fixture.attach(chosen, &scope);
        std::fs::write(base_dir.join("same.txt"), "base").unwrap();
        std::fs::write(chosen_dir.join("same.txt"), "chosen").unwrap();

        let resolved = fixture.resolve("same.txt", None).unwrap();

        assert_eq!(resolved.content, b"chosen");
    }

    /// D-033 item 3's final rung breaks a tie the resolver reports as
    /// ambiguous, and only that tie.
    #[test]
    fn unique_active_base_breaks_the_tie() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let (base_dir, _) = fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "base"), &scope);
        let mut worktree = spec(PROJECT_ONE, ATTACHMENT_TWO, "worktree");
        worktree.kind = AttachmentKind::Worktree;
        let (worktree_dir, _) = fixture.attach(worktree, &scope);
        std::fs::write(base_dir.join("same.txt"), "base").unwrap();
        std::fs::write(worktree_dir.join("same.txt"), "worktree").unwrap();

        let resolved = fixture.resolve("same.txt", None).unwrap();

        assert_eq!(resolved.content, b"base");
    }

    /// Two attached projects and no session pin is ambiguous, not a guess.
    #[test]
    fn two_attached_projects_without_a_session_are_ambiguous() {
        let fixture = Fixture::new();
        let one = Fixture::scope("repo-one");
        let two = Fixture::scope("repo-two");
        fixture.add_project(PROJECT_ONE, &one);
        fixture.add_project(PROJECT_TWO, &two);
        fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "one"), &one);
        fixture.attach(spec(PROJECT_TWO, ATTACHMENT_TWO, "two"), &two);

        let error = fixture.resolve("same.txt", None).unwrap_err().to_string();

        assert!(
            error.starts_with("error.project_selector_ambiguous"),
            "{error}"
        );
    }

    /// A catalog with projects but no attachment reports the attachment
    /// requirement rather than an empty registry.
    #[test]
    fn remote_only_host_requires_an_attachment() {
        let fixture = Fixture::new();
        fixture.add_project(PROJECT_ONE, &Fixture::scope("repo-one"));

        let error = fixture.resolve("same.txt", None).unwrap_err().to_string();

        assert!(
            error.starts_with("error.project_attachment_required"),
            "{error}"
        );
    }

    /// An absolute ref under an ACTIVE attachment resolves, on a project that
    /// has no compatibility record at all: its only attachment is a worktree,
    /// so the record projection omits it entirely. Resolution therefore
    /// cannot have gone through `ProjectRecord::canonical_path`.
    #[test]
    fn absolute_ref_resolves_under_an_attachment_with_no_compatibility_record() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let mut worktree = spec(PROJECT_ONE, ATTACHMENT_ONE, "worktree");
        worktree.kind = AttachmentKind::Worktree;
        let (dir, _) = fixture.attach(worktree, &scope);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn f() {}\n").unwrap();

        let resolved = fixture
            .resolve(dir.join("src/lib.rs").to_str().unwrap(), None)
            .unwrap();

        assert_eq!(resolved.relative_path, "src/lib.rs");
        assert_eq!(resolved.content, b"pub fn f() {}\n");
        assert_eq!(resolved.project_id, PROJECT_ONE);
    }

    /// A path recorded in the legacy ledger is store-key compatibility data,
    /// not authority. A file under a historical path that is no longer an
    /// active attachment must not resolve, even though the ledger still maps
    /// that path to a live project.
    #[test]
    fn stale_ledger_path_grants_no_authority() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "current"), &scope);
        let historical = fixture.root.join("historical");
        std::fs::create_dir_all(&historical).unwrap();
        std::fs::write(historical.join("secret.rs"), "old").unwrap();
        fixture.record_legacy_path(PROJECT_ONE, &historical);

        let error = fixture
            .resolve(historical.join("secret.rs").to_str().unwrap(), None)
            .unwrap_err()
            .to_string();

        assert!(
            error.starts_with("error.checkout_access.attachment_not_found"),
            "{error}"
        );
    }

    /// Traversal is refused before any authority is consulted, absolute and
    /// relative alike.
    #[test]
    fn traversal_is_refused() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let (dir, _) = fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "base"), &scope);

        let relative = fixture
            .resolve("../escape.rs", None)
            .unwrap_err()
            .to_string();
        assert!(relative.contains("without parent traversal"), "{relative}");

        let absolute = fixture
            .resolve(dir.join("../escape.rs").to_str().unwrap(), None)
            .unwrap_err()
            .to_string();
        assert!(absolute.contains("parent traversal"), "{absolute}");
    }

    /// A relative ref reaching outside the attachment through a symlink is
    /// refused by the lease's own path gate.
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let (dir, _) = fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "base"), &scope);
        let outside = fixture.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("escape")).unwrap();

        let error = fixture
            .resolve("escape/secret.txt", None)
            .unwrap_err()
            .to_string();

        assert!(
            error.starts_with("error.checkout_access.conservative_path_gate_denied"),
            "{error}"
        );
    }

    /// The file provider gates on `render_output`, its own capability bit,
    /// and nothing else.
    #[test]
    fn capability_denial_names_render_output() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let mut no_render = spec(PROJECT_ONE, ATTACHMENT_ONE, "base");
        no_render.render = false;
        fixture.attach(no_render, &scope);

        let error = fixture.resolve("file.rs", None).unwrap_err().to_string();

        assert!(
            error.starts_with("error.checkout_access.capability_denied"),
            "{error}"
        );
    }

    /// Identity lost between acquisition and revalidation fails the read
    /// rather than returning bytes read under an authority that no longer
    /// holds.
    #[test]
    fn revalidation_failure_fails_the_read() {
        let fixture = Fixture::new();
        let scope = Fixture::scope("repo-one");
        fixture.add_project(PROJECT_ONE, &scope);
        let (dir, _) = fixture.attach(spec(PROJECT_ONE, ATTACHMENT_ONE, "base"), &scope);
        std::fs::write(dir.join("file.rs"), "fn main() {}\n").unwrap();
        let broker = fixture.broker();
        let state = fixture.store.snapshot().unwrap();
        let target = catalog_attachment_target(&state, PROJECT_ONE, None).unwrap();
        let lease = broker
            .acquire(CheckoutAccessRequest {
                project_id: PROJECT_ONE.into(),
                attachment: CheckoutAttachmentSelector::AttachmentId(target.attachment_id),
                expected_scope: target.expected_scope,
                kind: CheckoutAccessKind::RenderFileProvider,
                intent: CheckoutAccessIntent::Read,
                source_lane: CheckoutAccessSourceLane::NativeAttachment,
            })
            .unwrap();
        std::fs::remove_file(dir.join(".bbox/local/checkout-id")).unwrap();

        let error = read_with_lease(&broker, PROJECT_ONE.into(), lease, Path::new("file.rs"))
            .unwrap_err()
            .to_string();

        assert!(error.contains("checkout_identity_mismatch"), "{error}");
    }
}

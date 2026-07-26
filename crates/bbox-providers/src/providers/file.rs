use std::collections::BTreeMap;
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use super::{
    EdgeFamilyExpectation, EntitySchemaView, EntityView, InspectableEntityProvider, Neighborhood,
    NextHop, ProviderCheckoutSelection, ProviderContext, empty_neighborhood_view, ensure_type,
    schema, truncate_label,
};
use bbox_corpus_core::entity_ref::{EntityRef, EntityType};
use bbox_corpus_core::identity::PublishedScope;
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind, CheckoutAccessRequest,
    CheckoutAccessSourceLane, CheckoutAttachmentSelector, ValidatedCheckoutLease,
};
use bbox_indexing::checkout_registry::CheckoutRow;
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

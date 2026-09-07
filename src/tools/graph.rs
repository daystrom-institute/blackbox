use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use bbox_corpus_core::identity::PublishedScope;
use bbox_corpus_core::project_catalog::AttachmentStatus;
use bbox_corpus_core::project_record::{ProjectRecord, ResolvedCheckoutScope};
use bbox_indexing::checkout_access::{
    CheckoutAccessBroker, CheckoutAccessError, CheckoutAccessIntent, CheckoutAccessKind,
    CheckoutAccessRequest, CheckoutAccessSourceLane, CheckoutAttachmentSelector,
    ValidatedCheckoutLease,
};
use bbox_indexing::checkout_registry::CheckoutRow;

use crate::mcp_tools;
use crate::mcp_tools::blame::BlameParams;
use crate::mcp_tools::bundle_evidence::BundleEvidenceParams;
use crate::mcp_tools::describe_schema::DescribeSchemaOptions;
use crate::mcp_tools::find_paths::FindPathsParams;
use crate::mcp_tools::inspect::InspectEntityParams;
use crate::mcp_tools::provenance::ProvenanceParams;
use crate::mcp_tools::provenance_plan::ProvenanceExportPlanParams;
use crate::mcp_tools::ref_size::RefSizeParams;
use crate::server::BlackboxServer;
use crate::{edge_index, entity_ref, git};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

const REF_SIZE_CAP: usize = 500;

/// One resolved checkout-file target. The carrier holds logical identity
/// only: a project id, an attachment selector, and the project-relative path
/// (or the instruction to derive it from the acquired lease). No
/// `ProjectRecord` and no host root participate, so nothing downstream can
/// re-derive authority from a path (plan section 6.9).
#[derive(Debug, Clone)]
struct CheckoutFileSelection {
    project_id: String,
    attachment: CheckoutAttachmentSelector,
    expected_scope: Option<PublishedScope>,
    source_lane: CheckoutAccessSourceLane,
    relative_path: FileRelativePath,
}

/// How a selection's project-relative path is determined.
#[derive(Debug, Clone)]
enum FileRelativePath {
    /// Known before acquisition: an explicit relative input, or a bridge
    /// candidate already matched against a registered root.
    Fixed(PathBuf),
    /// Stripped from the acquired lease's own project root. Catalog absolute
    /// selection resolves the attachment through the catalog's active
    /// attachments and strips that lease root, never a
    /// `ProjectRecord::canonical_path` (plan section 8, P5-E file item 6).
    UnderLeaseRoot(PathBuf),
}

#[derive(Debug)]
struct AcquiredCheckoutFile {
    lease: ValidatedCheckoutLease,
    relative_path: String,
    content: Vec<u8>,
}

fn checkout_access_error(error: CheckoutAccessError) -> anyhow::Error {
    anyhow!(
        "error.checkout_access.{}: {}",
        error.code.as_str(),
        error.diagnostic
    )
}

/// Acquire one lease for an operation named by project identity alone.
///
/// The bridge arm keeps the legacy two-step exactly: version-1 records carry
/// no scope, so the published scope is discoverable only through a
/// `PublisherConfigTreeRead` lease. The catalog arm names its attachment
/// natively and takes its scope from the catalog row.
fn acquire_selected_operation(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    project_id: &str,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<ValidatedCheckoutLease> {
    if !server.state.project_authority.is_bridge() {
        return crate::server::checkout_access::acquire_catalog_project_lease(
            server, broker, project_id, kind, intent,
        );
    }
    let discovery = acquire_scope_discovery(broker, project_id)?;
    let expected_scope = discovery.published_scope().cloned();
    drop(discovery);
    let lease = broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_string(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope,
            kind,
            intent,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(checkout_access_error)?;
    Ok(lease)
}

fn acquire_scope_discovery(
    broker: &CheckoutAccessBroker,
    project_id: &str,
) -> Result<ValidatedCheckoutLease> {
    broker
        .acquire(CheckoutAccessRequest {
            project_id: project_id.to_string(),
            attachment: CheckoutAttachmentSelector::Selected,
            expected_scope: None,
            kind: CheckoutAccessKind::PublisherConfigTreeRead,
            intent: CheckoutAccessIntent::Read,
            source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        })
        .map_err(checkout_access_error)
}

/// Selection-class boundary validation through the shared engine (phase-2
/// §9.2 B3). The bridge arm preserves the legacy error vocabulary for
/// unknown selectors; the catalog arm surfaces the §5.4 typed codes.
fn validate_explicit_project_selection(
    server: &crate::server::BlackboxServer,
    raw: &str,
) -> Result<()> {
    match server.resolve_project_selection(raw) {
        Ok(_) => Ok(()),
        Err(_) if server.state.project_authority.is_bridge() => {
            bail!("error.project_not_registered: requested project id is not registered")
        }
        Err(error) => Err(error),
    }
}

/// Enforce the monotonic blame-locality cut before the legacy adapter can
/// touch checkout authority. Corpus identity supplies an exact project id.
/// A path request is governed only when the session carries a stable project
/// selector; an unscoped raw path remains the explicitly named compatibility
/// lane until path locality has its own authority contract.
fn enforce_blame_locality_cutover(
    server: &crate::server::BlackboxServer,
    target: &mcp_tools::blame::BlameTargetIdentity,
) -> Result<()> {
    if server.state.project_authority.is_bridge() {
        return Ok(());
    }
    let project_id = match target {
        mcp_tools::blame::BlameTargetIdentity::ProjectFile { project_id, .. } => {
            Some(project_id.clone())
        }
        mcp_tools::blame::BlameTargetIdentity::File { .. } => server
            .session_surface_project()
            .and_then(|selector| server.validate_project_selection(&selector).ok()),
    };
    if project_id.as_deref().is_some_and(|project_id| {
        server
            .state
            .blame_locality_cutover
            .transport_governed(project_id)
    }) {
        bail!("error.blame_locality_required: this project's blame authority is checkout-local");
    }
    Ok(())
}

/// Internal slice matcher over records the handler boundary has already
/// validated through the shared engine (phase-2 §9.2): see
/// `validate_explicit_project_selection` at each explicit-project entry
/// point.
fn unique_project(projects: &[ProjectRecord], project_id: &str) -> Result<ProjectRecord> {
    let mut matches = projects
        .iter()
        .filter(|project| project.project_id == project_id);
    let project = matches.next().cloned().ok_or_else(|| {
        anyhow!("error.project_not_registered: requested project id is not registered")
    })?;
    if matches.next().is_some() {
        bail!("error.project_ambiguous: requested project id is not unique");
    }
    Ok(project)
}

fn safe_scope_root(checkout_root: &Path, scope: &PublishedScope) -> Result<PathBuf> {
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
        bail!("error.checkout_scope_invalid: project scope has an unsafe relative root");
    }
    Ok(checkout_root.join(relative))
}

fn selected_file_selection(project_id: String, relative_path: PathBuf) -> CheckoutFileSelection {
    CheckoutFileSelection {
        expected_scope: None,
        project_id,
        attachment: CheckoutAttachmentSelector::Selected,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        relative_path: FileRelativePath::Fixed(relative_path),
    }
}

fn checkout_file_selection(
    project_id: String,
    scope: PublishedScope,
    checkout_id: String,
    relative_path: PathBuf,
) -> CheckoutFileSelection {
    CheckoutFileSelection {
        project_id,
        attachment: CheckoutAttachmentSelector::CheckoutId(checkout_id),
        expected_scope: Some(scope),
        source_lane: CheckoutAccessSourceLane::LegacyCheckoutRegistry,
        relative_path: FileRelativePath::Fixed(relative_path),
    }
}

fn lexical_absolute_selection(
    broker: &CheckoutAccessBroker,
    input: &Path,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
    root_only: bool,
) -> Result<CheckoutFileSelection> {
    if !input.is_absolute()
        || input
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    {
        bail!(
            "error.checkout_path_invalid: selector must be an absolute lexical path without parent traversal"
        );
    }

    let mut candidates = Vec::<(usize, bool, CheckoutFileSelection)>::new();
    let mut discovered_scopes = HashMap::<String, Option<PublishedScope>>::new();
    for project in projects {
        let root = Path::new(&project.canonical_path);
        let Ok(relative) = input.strip_prefix(root) else {
            continue;
        };
        if root_only != relative.as_os_str().is_empty() {
            continue;
        }
        candidates.push((
            root.components().count(),
            false,
            selected_file_selection(project.project_id.clone(), relative.to_path_buf()),
        ));
    }
    for row in rows {
        let Some(scope) = row.published_scope() else {
            continue;
        };
        let root = safe_scope_root(Path::new(&row.checkout_dir), &scope)?;
        let Ok(relative) = input.strip_prefix(&root) else {
            continue;
        };
        if root_only != relative.as_os_str().is_empty() {
            continue;
        }
        for project in projects {
            if !discovered_scopes.contains_key(&project.project_id) {
                let discovery = acquire_scope_discovery(broker, &project.project_id)?;
                discovered_scopes.insert(
                    project.project_id.clone(),
                    discovery.published_scope().cloned(),
                );
                drop(discovery);
            }
            if !discovered_scopes
                .get(&project.project_id)
                .is_some_and(|discovered| discovered.as_ref() == Some(&scope))
            {
                continue;
            }
            candidates.push((
                root.components().count(),
                true,
                checkout_file_selection(
                    project.project_id.clone(),
                    scope.clone(),
                    row.checkout_id.clone(),
                    relative.to_path_buf(),
                ),
            ));
        }
    }

    let deepest = candidates
        .iter()
        .map(|(depth, _, _)| *depth)
        .max()
        .ok_or_else(|| {
            anyhow!(
                "error.checkout_attachment_not_found: selector is not in an exact registered project or checkout root"
            )
        })?;
    candidates.retain(|(depth, _, _)| *depth == deepest);
    if candidates.iter().any(|(_, checkout, _)| *checkout) {
        candidates.retain(|(_, checkout, _)| *checkout);
    }
    if candidates.len() != 1 {
        bail!(
            "error.checkout_attachment_ambiguous: selector matches more than one project attachment"
        );
    }
    Ok(candidates.pop().expect("one candidate").2)
}

fn absolute_file_selection(
    broker: &CheckoutAccessBroker,
    input: &Path,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
) -> Result<CheckoutFileSelection> {
    lexical_absolute_selection(broker, input, projects, rows, false)
}

fn relative_file_selection(
    broker: &CheckoutAccessBroker,
    relative: &Path,
    project_dir: Option<&str>,
    session_checkout: Option<&ResolvedCheckoutScope>,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
) -> Result<CheckoutFileSelection> {
    if relative.is_absolute() || relative.as_os_str().is_empty() {
        bail!("error.checkout_path_invalid: expected a non-empty relative file path");
    }
    if let Some(project_dir) = project_dir {
        let selector = Path::new(project_dir);
        let mut selection = lexical_absolute_selection(broker, selector, projects, rows, true)?;
        selection.relative_path = FileRelativePath::Fixed(relative.to_path_buf());
        return Ok(selection);
    }

    if let Some(session) = session_checkout {
        let project = unique_project(projects, &session.project_id)?;
        return Ok(checkout_file_selection(
            project.project_id,
            session.published_scope.clone(),
            session.checkout_id.clone(),
            relative.to_path_buf(),
        ));
    }

    match projects {
        [project] => Ok(selected_file_selection(
            project.project_id.clone(),
            relative.to_path_buf(),
        )),
        [] => bail!("error.project_not_registered: no registered project can resolve the file"),
        _ => bail!(
            "error.project_ambiguous: relative file requires project_dir or authoritative session checkout"
        ),
    }
}

/// The catalog project a path-free relative read acts on when the caller
/// named none: exactly one project carrying an active attachment. Zero and
/// many are typed refusals; nothing silently picks a project.
fn sole_attached_catalog_project(server: &BlackboxServer) -> Result<String> {
    let store = server
        .state
        .project_authority
        .catalog_store()
        .ok_or_else(|| {
            anyhow!("error.project_catalog_inactive: catalog authority is not active")
        })?;
    let state = store.snapshot().map_err(anyhow::Error::new)?;
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
            "error.project_selector_ambiguous: relative file requires project_dir or an authoritative session checkout"
        ),
    }
}

/// Catalog selection for a caller-supplied file path.
///
/// Absolute inputs resolve through the catalog's own active-attachment
/// containment arms (the `LegacyPath` selector routes into the catalog
/// resolver's path arms in catalog mode); no candidate is ever matched
/// against a `ProjectRecord::canonical_path` (plan section 8, P5-E file
/// items 5 and 6).
fn catalog_file_selection(
    server: &BlackboxServer,
    input: &str,
    project_dir: Option<&str>,
    session_checkout: Option<&ResolvedCheckoutScope>,
) -> Result<CheckoutFileSelection> {
    let path = Path::new(input);
    let traverses = |candidate: &Path| {
        candidate
            .components()
            .any(|component| matches!(component, Component::ParentDir))
    };
    if path.is_absolute() {
        if traverses(path) {
            bail!(
                "error.checkout_path_invalid: selector must be an absolute lexical path without parent traversal"
            );
        }
        return Ok(CheckoutFileSelection {
            project_id: String::new(),
            attachment: CheckoutAttachmentSelector::LegacyPath(input.to_owned()),
            expected_scope: None,
            source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
            relative_path: FileRelativePath::UnderLeaseRoot(path.to_path_buf()),
        });
    }
    if path.as_os_str().is_empty() || traverses(path) {
        bail!("error.checkout_path_invalid: expected a non-empty relative file path");
    }
    if let Some(project_dir) = project_dir {
        return Ok(CheckoutFileSelection {
            project_id: String::new(),
            attachment: CheckoutAttachmentSelector::LegacyPath(project_dir.to_owned()),
            expected_scope: None,
            source_lane: CheckoutAccessSourceLane::LegacyPathResolver,
            relative_path: FileRelativePath::Fixed(path.to_path_buf()),
        });
    }
    let project_id = match session_checkout {
        Some(session) => session.project_id.clone(),
        None => sole_attached_catalog_project(server)?,
    };
    let target = crate::server::checkout_access::catalog_attachment_target(server, &project_id)?;
    Ok(CheckoutFileSelection {
        project_id,
        attachment: CheckoutAttachmentSelector::AttachmentId(target.attachment_id),
        expected_scope: target.expected_scope,
        source_lane: CheckoutAccessSourceLane::NativeAttachment,
        relative_path: FileRelativePath::Fixed(path.to_path_buf()),
    })
}

fn acquire_file_selection(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    selection: CheckoutFileSelection,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<AcquiredCheckoutFile> {
    let project_id = selection.project_id;
    let lease = if selection.attachment == CheckoutAttachmentSelector::Selected {
        acquire_selected_operation(server, broker, &project_id, kind, intent)?
    } else {
        broker
            .acquire(CheckoutAccessRequest {
                project_id,
                attachment: selection.attachment,
                expected_scope: selection.expected_scope,
                kind,
                intent,
                source_lane: selection.source_lane,
            })
            .map_err(checkout_access_error)?
    };
    let relative = match selection.relative_path {
        FileRelativePath::Fixed(relative) => relative,
        FileRelativePath::UnderLeaseRoot(absolute) => absolute
            .strip_prefix(lease.project_root())
            .map_err(|_| {
                anyhow!(
                    "error.checkout_attachment_not_found: selector is not inside the selected attachment"
                )
            })?
            .to_path_buf(),
    };
    if relative.as_os_str().is_empty() {
        bail!("error.checkout_path_invalid: selector does not name a file inside the attachment");
    }
    let (_, content) = lease
        .read_relative_file(&relative)
        .map_err(checkout_access_error)?;
    Ok(AcquiredCheckoutFile {
        lease,
        relative_path: relative.to_string_lossy().into_owned(),
        content,
    })
}

fn file_selection(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    input: &str,
    project_dir: Option<&str>,
    session_checkout: Option<&ResolvedCheckoutScope>,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
) -> Result<CheckoutFileSelection> {
    if !server.state.project_authority.is_bridge() {
        return catalog_file_selection(server, input, project_dir, session_checkout);
    }
    let path = Path::new(input);
    if path.is_absolute() {
        absolute_file_selection(broker, path, projects, rows)
    } else {
        relative_file_selection(broker, path, project_dir, session_checkout, projects, rows)
    }
}

fn acquire_project_file(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    project_id: &str,
    indexed_path_hint: &Path,
    projects: &[ProjectRecord],
) -> Result<AcquiredCheckoutFile> {
    if server.state.project_authority.is_bridge() {
        return acquire_bridge_project_file(
            server,
            broker,
            project_id,
            indexed_path_hint,
            projects,
        );
    }
    // Catalog identity is path-free. An absolute hint has no record root to
    // strip and no attachment root may stand in for one: a hint rooted at some
    // other checkout would otherwise read a foreign file under this project's
    // identity.
    if indexed_path_hint.is_absolute() {
        bail!(
            "error.indexed_path_mismatch: project_file path hint is absolute and catalog identity carries no record root"
        );
    }
    if indexed_path_hint.as_os_str().is_empty() {
        bail!("error.indexed_path_invalid: project_file path hint does not name a file");
    }
    let lease = acquire_selected_operation(
        server,
        broker,
        project_id,
        CheckoutAccessKind::Blame,
        CheckoutAccessIntent::Read,
    )?;
    // Deliberately NO working-tree read. Catalog corpus-identity blame is
    // answered from the snapshot commit, and a file the corpus indexed may
    // legitimately be absent from the working tree now (deleted, or the
    // checkout moved on). Reading it here would refuse those cases for a
    // reason that has nothing to do with the question asked.
    Ok(AcquiredCheckoutFile {
        lease,
        relative_path: indexed_path_hint.to_string_lossy().into_owned(),
        content: Vec::new(),
    })
}

fn acquire_bridge_project_file(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    project_id: &str,
    indexed_path_hint: &Path,
    projects: &[ProjectRecord],
) -> Result<AcquiredCheckoutFile> {
    let project = unique_project(projects, project_id)?;
    let lease = acquire_selected_operation(
        server,
        broker,
        &project.project_id,
        CheckoutAccessKind::Blame,
        CheckoutAccessIntent::Read,
    )?;
    // P3-E: the stored path IS the project-relative path, so the normal arm is
    // a straight consume. The absolute-strip arm survives ONLY as a tagged
    // compat path for a pre-bump ref (a `file_path` fallback resolved against a
    // document written under the previous schema, or an operator-supplied
    // absolute hint); it is not the primary lane any more, and it never
    // fabricates a relative path from a foreign root.
    let relative = if indexed_path_hint.is_absolute() {
        tracing::debug!(
            project_id = %project.project_id,
            "compat: de-fabricating a relative path from a pre-path-free absolute hint"
        );
        indexed_path_hint
            .strip_prefix(Path::new(&project.canonical_path))
            .map(Path::to_path_buf)
            .map_err(|_| {
                anyhow!(
                    "error.indexed_path_mismatch: project_file path hint does not belong to its project"
                )
            })?
    } else {
        indexed_path_hint.to_path_buf()
    };
    if relative.as_os_str().is_empty() {
        bail!("error.indexed_path_invalid: project_file path hint does not name a file");
    }
    let (_, content) = lease
        .read_relative_file(&relative)
        .map_err(checkout_access_error)?;
    Ok(AcquiredCheckoutFile {
        lease,
        relative_path: relative.to_string_lossy().into_owned(),
        content,
    })
}

/// The exact commit corpus-identity blame must run against.
///
/// The pinned Git overlay is the corpus snapshot's Git evidence: it names the
/// attachment whose head produced the project's commit edges and the head it
/// observed. Catalog corpus-identity blame is bound to that commit, not
/// merely checked against it. Two failures are refusals rather than
/// fallbacks, because either one would silently answer a question about the
/// indexed snapshot using unrelated current history (plan section 8, P5-E
/// blame items 4 and 5):
///
/// - no overlay: there is no evidence of WHICH snapshot the corpus indexed,
///   so there is no commit to be faithful to;
/// - overlay commit absent from the selected checkout: the attachment cannot
///   answer for that snapshot even though it was selected for the project.
///
/// A checkout that CONTAINS the commit but has since advanced is fine and is
/// the ordinary case: blame runs at the recorded commit regardless of where
/// HEAD has moved.
fn snapshot_commit_for_blame(
    git_overlays: &std::collections::BTreeMap<
        String,
        bbox_corpus_core::git_overlay::GitOverlaySelector,
    >,
    project_id: &str,
    checkout_root: &Path,
) -> Result<String> {
    let Some(overlay) = git_overlays.get(project_id) else {
        bail!(
            "error.blame_snapshot_unavailable: no Git snapshot evidence is recorded for this project, so blame cannot be bound to the indexed corpus snapshot"
        );
    };
    bbox_corpus_core::git::resolve_commit(checkout_root, &overlay.repo_head).ok_or_else(|| {
        anyhow!(
            "error.blame_commit_mismatch: the selected attachment does not contain the commit the corpus snapshot was indexed at"
        )
    })
}

struct SessionBlameAuthority {
    project_id: String,
    scope: PublishedScope,
    workspace_id: String,
    observation_authority: bbox_indexing::blame_locality_observations::BlameLocalityAuthorityV1,
}

fn session_blame_authority(server: &BlackboxServer) -> Result<SessionBlameAuthority> {
    if let Some(grant) = server.authoritative_session_workspace_binding() {
        if !grant.is_live_now() {
            bail!("error.blame_locality_binding: workspace binding has expired");
        }
        return Ok(SessionBlameAuthority {
            project_id: grant.project_id.clone(),
            scope: grant.scope.clone(),
            workspace_id: grant.workspace_id.as_str().to_string(),
            observation_authority:
                bbox_indexing::blame_locality_observations::BlameLocalityAuthorityV1::ManagedWorkspace,
        });
    }
    if let Some(grant) = server.authoritative_operator_blame_binding() {
        return Ok(SessionBlameAuthority {
            project_id: grant.project_id.clone(),
            scope: grant.scope.clone(),
            workspace_id: grant.workspace_id.as_str().to_string(),
            observation_authority:
                bbox_indexing::blame_locality_observations::BlameLocalityAuthorityV1::Operator,
        });
    }
    bail!("error.blame_locality_binding: blame locality requires checkout-side authority")
}

/// Build the checkout owner's blame plan from corpus identity only.
///
/// Unlike `snapshot_commit_for_blame`, this does not prove the commit through
/// a daemon-visible object database. The workspace-bound harness is the
/// checkout owner and performs that proof while executing the plan.
fn workspace_blame_plan(
    server: &BlackboxServer,
    target: &mcp_tools::blame::BlameTargetIdentity,
    git_overlays: &std::collections::BTreeMap<
        String,
        bbox_corpus_core::git_overlay::GitOverlaySelector,
    >,
) -> Result<bbox_corpus_core::blame_transport::BlameExecutionPlanV1> {
    use bbox_corpus_core::blame_transport::{
        BLAME_TRANSPORT_VERSION, BlameExecutionPlanV1, BlamePlanTargetV1,
    };

    let grant = session_blame_authority(server)?;
    let target = match target {
        mcp_tools::blame::BlameTargetIdentity::ProjectFile {
            project_id,
            indexed_path_hint,
            line,
            byte_offset,
        } => {
            if project_id != &grant.project_id {
                bail!(
                    "error.blame_locality_scope: corpus entity belongs to a different project than the bound workspace"
                );
            }
            validate_explicit_project_selection(server, project_id)?;
            if indexed_path_hint.is_absolute() {
                bail!(
                    "error.indexed_path_mismatch: project_file path hint is absolute and cannot cross the blame locality boundary"
                );
            }
            let relative = indexed_path_hint.to_string_lossy().replace('\\', "/");
            let commit = git_overlays
                .get(project_id)
                .map(|overlay| overlay.repo_head.clone())
                .context(
                    "error.blame_snapshot_unavailable: no Git snapshot evidence is recorded for this project, so blame cannot be bound to the indexed corpus snapshot",
                )?;
            BlamePlanTargetV1::ProjectSnapshot {
                project_relative_path: relative.clone(),
                display_path: relative,
                line: *line,
                byte_offset: *byte_offset,
                commit,
            }
        }
        mcp_tools::blame::BlameTargetIdentity::File { input_path, line } => {
            BlamePlanTargetV1::WorkspacePath {
                input_path: input_path.clone(),
                line: *line,
            }
        }
    };
    let plan = BlameExecutionPlanV1 {
        version: BLAME_TRANSPORT_VERSION,
        project_id: grant.project_id,
        scope: grant.scope,
        workspace_id: grant.workspace_id,
        target,
    };
    plan.validate()?;
    Ok(plan)
}

fn blame_observation_target(
    plan: &bbox_corpus_core::blame_transport::BlameExecutionPlanV1,
) -> bbox_indexing::blame_locality_observations::BlameLocalityTargetV1 {
    match &plan.target {
        bbox_corpus_core::blame_transport::BlamePlanTargetV1::WorkspacePath { .. } => {
            bbox_indexing::blame_locality_observations::BlameLocalityTargetV1::Path
        }
        bbox_corpus_core::blame_transport::BlamePlanTargetV1::ProjectSnapshot { .. } => {
            bbox_indexing::blame_locality_observations::BlameLocalityTargetV1::Entity
        }
    }
}

/// The project ids one legacy Git-note operation covers.
///
/// Catalog mode selects the COMPLETE catalog set, remote-only projects
/// included. Narrowing to the attached compatibility projection would turn
/// an all-project operation into silent partial success, which D-020's
/// governing rule and plan section 4.20 both forbid: the missing attachment
/// must surface as the first typed refusal instead.
fn requested_provenance_projects(
    server: &BlackboxServer,
    params: &ProvenanceParams,
    projects: &[ProjectRecord],
) -> Result<Vec<String>> {
    if !server.state.project_authority.is_bridge() {
        if let Some(project_id) = params.project_id.as_deref() {
            return Ok(vec![server.validate_project_selection(project_id)?]);
        }
        let snapshot = server.state.records_provider.records_snapshot();
        return Ok(snapshot.corpus_project_ids.iter().cloned().collect());
    }
    if let Some(project_id) = params.project_id.as_deref() {
        return Ok(vec![unique_project(projects, project_id)?.project_id]);
    }
    let mut seen = HashSet::new();
    for project in projects {
        if !seen.insert(project.project_id.as_str()) {
            bail!("error.project_ambiguous: registered project ids are not unique");
        }
    }
    Ok(projects
        .iter()
        .map(|project| project.project_id.clone())
        .collect())
}

fn acquire_provenance_projects(
    server: &BlackboxServer,
    broker: &CheckoutAccessBroker,
    params: &ProvenanceParams,
    projects: &[ProjectRecord],
    intent: CheckoutAccessIntent,
) -> Result<(
    Vec<ValidatedCheckoutLease>,
    Vec<mcp_tools::provenance::ProvenanceProject>,
)> {
    let requested = requested_provenance_projects(server, params, projects)?;
    // Evaluate the complete target set before acquiring the first lease. A
    // catalog all-project operation must not partially touch LegacyLocal
    // checkouts and then discover a marker-covered Published project later in
    // iteration order.
    for project_id in &requested {
        if server
            .state
            .git_transport_governs_project(project_id)
            .map_err(|error| {
                anyhow!(
                    "error.provenance_transport_authoritative: cutover authority could not be classified for {project_id}: {error}"
                )
            })?
        {
            bail!(
                "error.provenance_transport_authoritative: project {project_id} is governed by producer provenance transport"
            );
        }
    }
    let mut leases = Vec::with_capacity(requested.len());
    let mut inputs = Vec::with_capacity(requested.len());
    for project_id in requested {
        // First typed refusal returns: no project is skipped and no partial
        // result is assembled (plan section 4.20).
        let lease = acquire_selected_operation(
            server,
            broker,
            &project_id,
            CheckoutAccessKind::ProvenanceNoteIo,
            intent,
        )?;
        inputs.push(mcp_tools::provenance::ProvenanceProject {
            project_id,
            project_root: lease.project_root().to_path_buf(),
        });
        leases.push(lease);
    }
    Ok((leases, inputs))
}

/// The projects a legacy provenance operation actually leased.
fn leased_provenance_projects(
    inputs: &[mcp_tools::provenance::ProvenanceProject],
) -> BTreeSet<String> {
    inputs
        .iter()
        .map(|input| input.project_id.clone())
        .collect()
}

/// Authorize one legacy provenance anchor for re-identification.
///
/// Authority is the set of projects this operation already leased, not the
/// compatibility projection. The projection carries only each project's
/// unique active BASE attachment, so a catalog project attached solely
/// through a worktree is absent from it; resolving through it refused that
/// project's anchors even though its `ProvenanceNoteIo` lease had just
/// succeeded, which is the P5-E residual this closes.
fn authorize_legacy_provenance_target(
    authorized: &BTreeSet<String>,
    project_id: &str,
) -> Result<()> {
    if !authorized.contains(project_id) {
        bail!(
            "error.project_mismatch: provenance target names a project this import did not lease"
        );
    }
    Ok(())
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::graph_tools()
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct EdgeCompactParams {
    /// Project id whose legacy sidecar should be compacted.
    pub project_id: String,
    /// Apply the compaction. Defaults to false, returning a dry-run summary.
    pub apply: Option<bool>,
    /// Rebuild the in-memory EdgeIndex after applying. With apply=true, this
    /// also works when compaction is already a no-op. Uses a sidecar-only
    /// rebuild. Defaults to false because graph rebuilds can be expensive while
    /// legacy sidecars are still large.
    pub rebuild: Option<bool>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct DescribeSchemaParams {
    /// Include the installed-agent catalog. Default false: compact orientation
    /// returns graph vocabulary/traversal tips without the potentially large
    /// agent list.
    pub include_agents: Option<bool>,
    /// Convenience mode. `full` includes installed agents; `orientation` keeps
    /// the compact default. `agents` is a deprecated alias for `full`.
    pub mode: Option<String>,
    /// Exact schema body pages, including any requested agent catalog. Changed
    /// population or catalog evidence refuses continuation.
    pub cursor: Option<String>,
    /// Exact body bytes, clamped to 4..=4096. Oversized replies also start a body page.
    pub body_limit: Option<usize>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphListParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: Option<String>,
    /// Visibility policy: published, own, or all. `provisional` is the
    /// canonical spelling; `visibility` is accepted as a deprecated alias
    /// for older callers and recordings.
    #[serde(default, alias = "visibility")]
    pub provisional: Option<String>,
    /// Maximum graphs per page (1..=100, default 20). Pages also obey a
    /// serialized byte budget.
    pub limit: Option<usize>,
    /// Continuation offset from a previous page's next_offset. Nonzero
    /// offsets require expected_view_stamp.
    pub offset: Option<usize>,
    /// View stamp from the previous page. A changed graph view refuses the
    /// continuation; restart at offset 0.
    pub expected_view_stamp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphDescribeParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: String,
    pub graph_id: String,
    /// Visibility policy: published, own, or all. `provisional` is the
    /// canonical spelling; `visibility` is accepted as a deprecated alias
    /// for older callers and recordings.
    #[serde(default, alias = "visibility")]
    pub provisional: Option<String>,
    /// Authority-plane selector: published, provisional, or connector.
    /// Filters the variants visibility already returned; never widens it.
    pub source: Option<String>,
    /// Checkout identity of one provisional variant, from its list entry.
    pub checkout_id: Option<String>,
    /// Generation content hash, from its list entry. Distinct
    /// sources/checkouts can repeat one hash, so combine it with source or
    /// checkout_id when they do.
    pub expected_content_hash: Option<String>,
    /// detail=summary (default) keeps the response compact; detail=schema
    /// and detail=descriptor recover the exact JSON bodies in bounded pages.
    pub detail: Option<String>,
    /// Body continuation cursor; only valid with detail=schema or
    /// descriptor. Cursors are content-bound to the exact selected variant:
    /// any graph, selection, or content change rejects them.
    pub cursor: Option<String>,
    /// Body page size in UTF-8 bytes (4..=4096, default 4096).
    pub body_limit: Option<usize>,
    /// Variant page size for multi-variant summaries (1..=100, default 20).
    /// Pages also obey a serialized byte budget.
    pub variant_limit: Option<usize>,
    /// Variant continuation offset from a previous page's next_offset.
    /// Nonzero offsets require expected_view_stamp.
    pub variant_offset: Option<usize>,
    /// View stamp from the previous variant page. A changed variant set
    /// refuses the continuation; restart at variant_offset 0.
    pub expected_view_stamp: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphValidateParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: String,
    pub graph_id: String,
    /// Visibility policy: published, own, or all. `provisional` is the
    /// canonical spelling; `visibility` is accepted as a deprecated alias
    /// for older callers and recordings.
    #[serde(default, alias = "visibility")]
    pub provisional: Option<String>,
    /// Authority-plane selector: published, provisional, or connector.
    /// Filters the variants visibility already returned; never widens it.
    pub source: Option<String>,
    /// Checkout identity of one provisional variant, from its list entry.
    pub checkout_id: Option<String>,
    /// Generation content hash, from its list entry. Distinct
    /// sources/checkouts can repeat one hash, so combine it with source or
    /// checkout_id when they do.
    pub expected_content_hash: Option<String>,
    /// detail=summary (default) returns bounded error pages; detail=errors
    /// recovers the complete error array as exact JSON pages.
    pub detail: Option<String>,
    /// Body continuation cursor; only valid with detail=errors. Cursors are
    /// content-bound to the exact selected variant: any graph, selection,
    /// or content change rejects them.
    pub cursor: Option<String>,
    /// Body page size in UTF-8 bytes (4..=4096, default 4096).
    pub body_limit: Option<usize>,
    /// Error page offset from a previous page's next_error_offset. Nonzero
    /// offsets require expected_error_stamp.
    pub error_offset: Option<usize>,
    /// Maximum validation errors per page (1..=100, default 20).
    pub error_limit: Option<usize>,
    /// Error stamp from a previous page. A changed error set refuses the
    /// continuation; restart at error_offset 0.
    pub expected_error_stamp: Option<String>,
    /// Variant summary page size (1..=100, default 20).
    pub variant_limit: Option<usize>,
    /// Variant continuation offset; nonzero requires expected_view_stamp.
    pub variant_offset: Option<usize>,
    /// Stamp from the previous variant page; changed evidence refuses continuation.
    pub expected_view_stamp: Option<String>,
}

impl DescribeSchemaParams {
    fn include_agents_resolved(&self) -> Result<bool> {
        let from_mode = match self.mode.as_deref() {
            None | Some("orientation") => false,
            Some("full" | "agents") => true,
            Some(_) => bail!("Invalid schema mode; use orientation or full"),
        };
        Ok(self.include_agents.unwrap_or(from_mode))
    }
}

#[tool_router(router = graph_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_inspect_entity",
        description = "Inspect properties and targeted edges. Filter edge_types and direction; per_type_limit=0 reads properties only. property_mode selects summary, smart, or full. Follow edge_page.next_cursor for more edges; property retrieves exact text in pages."
    )]
    pub(crate) async fn bbox_inspect_entity(
        &self,
        Parameters(p): Parameters<InspectEntityParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_inspect_entity", move || {
            let entity_ref = match entity_ref::EntityRef::parse(&p.entity_ref) {
                Ok(entity_ref) => entity_ref,
                Err(err) => {
                    let output = mcp_tools::inspect::bad_input(&p.entity_ref, err.to_string());
                    let structured = serde_json::from_str(&output)?;
                    return Ok((output, structured));
                }
            };
            let knowledge_view = server.session_knowledge_view(None, p.provisional.as_deref())?;
            let read_view = server.state.complete_code_read_view()?;
            let edge_index = read_view.edge_index.as_ref();
            if matches!(
                &entity_ref,
                entity_ref::EntityRef::ProjectFile { .. }
                    | entity_ref::EntityRef::ProjectFileV2 { .. }
            ) && !server
                .state
                .idx
                .read()
                .is_active_code_entity_for_with_searcher(
                    &p.entity_ref,
                    &read_view.active_selectors,
                    &read_view.searcher,
                )
            {
                let output = mcp_tools::inspect::not_found(
                    &entity_ref,
                    mcp_tools::inspect::similar_refs(edge_index, &entity_ref),
                );
                return knowledge_view.enrich_json_response(output);
            }
            let provider_ctx = server
                .provider_context()
                .with_knowledge_view(&knowledge_view.knowledge)
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher)
                .with_project_graph_resolver(&server, p.provisional.as_deref());
            let output =
                mcp_tools::inspect::inspect_entity(&p, &provider_ctx, &entity_ref, edge_index)?;
            knowledge_view.enrich_json_response(output)
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_list",
        description = "List visible project graphs in bounded pages (default 20, max 100, also byte-budgeted) ordered by graph_id, source, then checkout. Continue with next_offset plus expected_view_stamp."
    )]
    pub(crate) async fn bbox_project_graph_list(
        &self,
        Parameters(p): Parameters<ProjectGraphListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_list", move || {
            let offset = p.offset.unwrap_or(0);
            let (_, graphs, view_stamp) = server.project_graph_inventory_domain(
                p.project.as_deref(),
                p.provisional.as_deref(),
            )?;
            if offset > 0 && p.expected_view_stamp.is_none() {
                bail!("error.graph_view_stamp_required: continue with expected_view_stamp from the previous response");
            }
            if p
                .expected_view_stamp
                .as_deref()
                .is_some_and(|expected| expected != view_stamp.as_str())
            {
                bail!("error.graph_view_changed: graph view changed since the previous page; restart at offset=0 without expected_view_stamp");
            }
            let rows = graphs
                .into_iter()
                .map(|graph| serde_json::to_value(&graph))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let mut page = bbox_corpus_core::response_page::collection_page(
                rows,
                "graphs",
                p.limit,
                p.offset,
            )?;
            page["status"] = json!("ok");
            page["provisional"] = json!(p.provisional);
            page["view_stamp"] = json!(view_stamp);
            page["order"] = json!("graph_id_source_checkout_asc");
            page["continuation_note"] = json!(
                "Live view state, not a snapshot: published installs and provisional overlays replace whole entries, so a changed view refuses nonzero offsets instead of paging a different inventory."
            );
            Ok(serde_json::to_string(&page)?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_describe",
        description = "Describe one visible project graph. detail=summary (default) stays compact: identity, generation, authority plane, retrieval state, and schema counts without the schema body; multi-variant summaries page with totals. detail=schema or detail=descriptor recovers the exact JSON in bounded body pages."
    )]
    pub(crate) async fn bbox_project_graph_describe(
        &self,
        Parameters(p): Parameters<ProjectGraphDescribeParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_describe", move || {
            let detail = crate::project_graph_read::GraphDescribeDetail::parse(p.detail.as_deref())?;
            let selector = crate::project_graph_read::GraphVariantSelector::parse(
                p.source.as_deref(),
                p.checkout_id.as_deref(),
                p.expected_content_hash.as_deref(),
            )?;
            match detail {
                crate::project_graph_read::GraphDescribeDetail::Summary => {
                    if p.cursor.is_some() {
                        bail!("error.bad_input: cursor requires detail=schema or detail=descriptor");
                    }
                    if p.body_limit.is_some() {
                        bail!("error.bad_input: body_limit requires detail=schema or detail=descriptor");
                    }
                    let (mut graphs, view_stamp) = server.project_graph_describe_domain(
                        &p.project,
                        &p.graph_id,
                        p.provisional.as_deref(),
                    )?;
                    if let Some(selector) = &selector {
                        graphs.retain(|description| {
                            selector.matches_parts(
                                description.summary.source,
                                description.summary.checkout_id.as_deref(),
                                description.summary.content_hash.as_str(),
                            )
                        });
                        if graphs.is_empty() {
                            bail!(
                                "error.not_found: no visible variant of graph `{}` matches {}",
                                p.graph_id,
                                selector.describe()
                            );
                        }
                    }
                    let variant_offset = p.variant_offset.unwrap_or(0);
                    let paged = variant_offset > 0
                        || p.variant_limit.is_some()
                        || p.expected_view_stamp.is_some();
                    if variant_offset > 0 && p.expected_view_stamp.is_none() {
                        bail!("error.graph_view_stamp_required: continue with expected_view_stamp from the previous response");
                    }
                    if p
                        .expected_view_stamp
                        .as_deref()
                        .is_some_and(|expected| expected != view_stamp.as_str())
                    {
                        bail!("error.graph_view_changed: graph variants changed since the previous page; restart at variant_offset=0 without expected_view_stamp");
                    }
                    if !paged && graphs.len() == 1 {
                        let mut page =
                            serde_json::to_value(graphs.pop().expect("one graph described"))?;
                        page["status"] = json!("ok");
                        page["detail"] = json!("summary");
                        page["detail_hint"] = json!(
                            "Exact schema: bbox_project_graph_describe(project,graph_id,detail=\"schema\"); descriptor: detail=\"descriptor\"; continue with cursor=body.next_cursor. The summary carries the excluded-type count; the exact list lives in the schema body."
                        );
                        return Ok(serde_json::to_string_pretty(&page)?);
                    }
                    let rows = graphs
                        .into_iter()
                        .map(|graph| serde_json::to_value(&graph))
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    let mut page = bbox_corpus_core::response_page::collection_page(
                        rows,
                        "graphs",
                        p.variant_limit,
                        p.variant_offset,
                    )?;
                    page["status"] = json!("ok");
                    page["detail"] = json!("summary");
                    page["view_stamp"] = json!(view_stamp);
                    page["order"] = json!("source_checkout_content_asc");
                    page["continuation_note"] = json!(
                        "Live view state, not a snapshot: published installs and provisional overlays replace whole entries, so a changed variant set refuses nonzero offsets instead of paging different variants."
                    );
                    page["detail_hint"] = json!(
                        "Exact schema: detail=\"schema\"; descriptor: detail=\"descriptor\"; select one variant with source, checkout_id, and expected_content_hash when several are visible; continue with cursor=body.next_cursor."
                    );
                    Ok(serde_json::to_string(&page)?)
                }
                crate::project_graph_read::GraphDescribeDetail::Schema
                | crate::project_graph_read::GraphDescribeDetail::Descriptor => {
                    if p.variant_limit.is_some() {
                        bail!("error.bad_input: variant_limit applies only to detail=summary");
                    }
                    if p.variant_offset.is_some() {
                        bail!("error.bad_input: variant_offset applies only to detail=summary");
                    }
                    if p.expected_view_stamp.is_some() {
                        bail!("error.bad_input: expected_view_stamp applies only to detail=summary");
                    }
                    let read = server.project_graph_detail_domain(
                        &p.project,
                        &p.graph_id,
                        p.provisional.as_deref(),
                        detail,
                        selector.as_ref(),
                    )?;
                    let scope = format!(
                        "{}:{}:{}:{}:{}:{}:{}",
                        read.project_id,
                        read.provisional_mode,
                        p.graph_id,
                        read.source,
                        read.checkout_id.as_deref().unwrap_or("-"),
                        read.generation.content_hash,
                        detail.as_str()
                    );
                    let body = super::body_page::json_body_page(
                        &scope,
                        &read.body,
                        p.cursor.as_deref(),
                        p.body_limit,
                    )?;
                    Ok(serde_json::to_string_pretty(&json!({
                        "status": "ok",
                        "detail": detail.as_str(),
                        "summary": read.summary,
                        "generation": read.generation,
                        "body": body,
                    }))?)
                }
            }
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_validate",
        description = "Validate one visible project graph. detail=summary (default) pages error rows (default 20, max 100) with errors_total; detail=errors recovers the complete error array as exact JSON body pages."
    )]
    pub(crate) async fn bbox_project_graph_validate(
        &self,
        Parameters(p): Parameters<ProjectGraphValidateParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_validate", move || {
            let detail = crate::project_graph_read::GraphValidateDetail::parse(p.detail.as_deref())?;
            let selector = crate::project_graph_read::GraphVariantSelector::parse(
                p.source.as_deref(),
                p.checkout_id.as_deref(),
                p.expected_content_hash.as_deref(),
            )?;
            match detail {
                crate::project_graph_read::GraphValidateDetail::Summary => {
                    if p.cursor.is_some() {
                        bail!("error.bad_input: cursor requires detail=errors");
                    }
                    if p.body_limit.is_some() {
                        bail!("error.bad_input: body_limit requires detail=errors");
                    }
                    let error_offset = p.error_offset.unwrap_or(0);
                    let error_limit = p.error_limit.unwrap_or(20).clamp(1, 100);
                    let graphs = server.project_graph_validate_domain(
                        &p.project,
                        &p.graph_id,
                        p.provisional.as_deref(),
                        selector.as_ref(),
                        error_offset,
                        error_limit,
                    )?;
                    let paging = error_offset > 0 || p.expected_error_stamp.is_some();
                    if paging {
                        if graphs.len() > 1 {
                            bail!(
                                "error.project_graph_ambiguous: error paging needs exactly one visible variant; select one with source, checkout_id, and expected_content_hash (or narrow provisional to published or own)"
                            );
                        }
                        let Some(graph) = graphs.first() else {
                            bail!("error.not_found: graph `{}` was not found", p.graph_id);
                        };
                        if p.expected_error_stamp.is_none() {
                            bail!("error.graph_error_stamp_required: continue with expected_error_stamp from the previous response");
                        }
                        if p
                            .expected_error_stamp
                            .as_deref()
                            .is_some_and(|expected| expected != graph.error_stamp.as_str())
                        {
                            bail!("error.graph_errors_changed: validation errors changed since the previous page; restart at error_offset=0 without expected_error_stamp");
                        }
                    }
                    let scope = json!([p.project, p.graph_id, p.provisional, p.source,
                        p.checkout_id, p.expected_content_hash, error_offset, error_limit]);
                    let stamp = format!("{:x}", Sha256::digest(serde_json::to_vec(
                        &json!([scope, graphs]))?));
                    let offset = p.variant_offset.unwrap_or(0);
                    if offset > 0 && p.expected_view_stamp.is_none() {
                        bail!("error.graph_view_stamp_required: continue with expected_view_stamp from the previous response");
                    }
                    if p.expected_view_stamp.as_deref().is_some_and(|v| v != stamp) {
                        bail!("error.graph_view_changed: validation variants changed; restart at variant_offset=0 without expected_view_stamp");
                    }
                    let rows = graphs.into_iter().map(serde_json::to_value)
                        .collect::<std::result::Result<Vec<_>, _>>()?;
                    let mut page = bbox_corpus_core::response_page::collection_page(
                        rows, "graphs", p.variant_limit, Some(offset))?;
                    page["status"] = json!("ok");
                    page["detail"] = json!("summary");
                    page["view_stamp"] = json!(stamp);
                    page["detail_hint"] = json!("Continue variants with variant_offset=next_offset and expected_view_stamp=view_stamp. Exact errors: detail=errors with one variant selected by source, checkout_id and expected_content_hash.");
                    Ok(page.to_string())
                }
                crate::project_graph_read::GraphValidateDetail::Errors => {
                    if p.variant_limit.is_some() || p.variant_offset.is_some() || p.expected_view_stamp.is_some() {
                        bail!("error.bad_input: variant paging applies only to detail=summary");
                    }
                    if p.error_offset.is_some() {
                        bail!("error.bad_input: error_offset applies only to detail=summary");
                    }
                    if p.error_limit.is_some() {
                        bail!("error.bad_input: error_limit applies only to detail=summary");
                    }
                    if p.expected_error_stamp.is_some() {
                        bail!("error.bad_input: expected_error_stamp applies only to detail=summary");
                    }
                    let read = server.project_graph_validation_errors_domain(
                        &p.project,
                        &p.graph_id,
                        p.provisional.as_deref(),
                        selector.as_ref(),
                    )?;
                    let scope = format!(
                        "{}:{}:{}:{}:{}:{}:errors",
                        read.project_id,
                        read.provisional_mode,
                        p.graph_id,
                        read.source,
                        read.checkout_id.as_deref().unwrap_or("-"),
                        read.generation.content_hash
                    );
                    let body = super::body_page::json_body_page(
                        &scope,
                        &read.body,
                        p.cursor.as_deref(),
                        p.body_limit,
                    )?;
                    Ok(serde_json::to_string_pretty(&json!({
                        "status": "ok",
                        "detail": "errors",
                        "summary": read.summary,
                        "generation": read.generation,
                        "body": body,
                    }))?)
                }
            }
        })
        .await
    }

    #[tool(
        name = "bbox_describe_schema",
        description = "Orient to entity types and edge families. mode=full expands fields and agents; include_agents=false omits agents. body_limit/cursor recovers exact schema JSON; oversized replies automatically start body pages."
    )]
    pub(crate) fn bbox_describe_schema(
        &self,
        Parameters(p): Parameters<DescribeSchemaParams>,
    ) -> CallToolResult {
        Self::run("bbox_describe_schema", || {
            let include_agents = p.include_agents_resolved()?;
            let read_view = self.state.complete_code_read_view()?;
            let agents = include_agents
                .then(|| self.build_agent_schema_entries())
                .unwrap_or_default();
            let rendered = mcp_tools::describe_schema::describe_schema_with_options(
                &self.describe_schema_counts_from_view(&read_view),
                &agents,
                DescribeSchemaOptions {
                    include_agents,
                    compact: !include_agents && p.mode.as_deref() != Some("full"),
                },
            )?;
            let scope = json!(["schema", include_agents, p.mode]).to_string();
            let page = bbox_corpus_core::response_page::bounded_json_response(
                &scope,
                serde_json::from_str(&rendered)?,
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok(page.to_string())
        })
    }

    #[tool(
        name = "bbox_find_paths",
        description = "Find direction-preserving paths to an exact ref (to) or entity type (to_type); a target is required. Filter edge_types and use a small max_depth. Fanout omissions and evidence freshness are explicit. Pass returned path IDs to bbox_bundle_evidence."
    )]
    pub(crate) async fn bbox_find_paths(
        &self,
        Parameters(p): Parameters<FindPathsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_find_paths", move || {
            let read_view = server.state.complete_code_read_view()?;
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher)
                .with_project_graph_resolver(&server, p.provisional.as_deref());
            mcp_tools::find_paths::find_paths(
                &p,
                &provider_ctx,
                edge_index,
                &mut server.state.path_cache.write(),
            )
        })
        .await
    }

    #[tool(
        name = "bbox_bundle_evidence",
        description = "Bundle entity refs and cached paths with provenance and freshness. Properties default to summary; full/none are explicit. body_limit/cursor recovers exact bundle JSON, and oversized replies automatically start body pages."
    )]
    pub(crate) async fn bbox_bundle_evidence(
        &self,
        Parameters(p): Parameters<BundleEvidenceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_bundle_evidence", move || {
            let read_view = server.state.complete_code_read_view()?;
            let knowledge_view = server.session_knowledge_view(None, p.provisional.as_deref())?;
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_knowledge_view(&knowledge_view.knowledge)
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher)
                .with_project_graph_resolver(&server, p.provisional.as_deref());
            let output = mcp_tools::bundle_evidence::bundle_evidence(
                &p,
                &provider_ctx,
                edge_index,
                &mut server.state.path_cache.write(),
            )?;
            let (_, enriched) = knowledge_view.enrich_json_response(output)?;
            let scope = json!([
                "bundle",
                p.question,
                p.entity_refs,
                p.path_ids,
                p.provisional,
                p.property_mode
            ])
            .to_string();
            let bounded = bbox_corpus_core::response_page::bounded_json_response(
                &scope,
                enriched,
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok((bounded.to_string(), bounded))
        })
        .await
    }

    #[tool(
        name = "bbox_ref_size",
        description = "Measure entity payload bytes using authoritative indexed or checkout reads. body_limit/cursor recovers exact result JSON; oversized replies start body pages automatically. Each page remeasures the selected refs, and changed evidence refuses continuation."
    )]
    pub(crate) async fn bbox_ref_size(
        &self,
        Parameters(p): Parameters<RefSizeParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_ref_size", move || {
            mcp_tools::ref_size::validate_response_params(&p)?;
            let read_view = server.state.complete_code_read_view()?;
            let projects = server.state.records_provider.records_snapshot().records;
            let checkout_rows = server.state.checkout_registry.read().rows().to_vec();
            let session_checkout = server.authoritative_session_checkout();
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let mut acquired_files = Vec::new();
            let mut validated_files = HashMap::new();
            for raw in p.refs.iter().take(REF_SIZE_CAP) {
                let Ok(entity_ref::EntityRef::File { path }) = entity_ref::EntityRef::parse(raw)
                else {
                    continue;
                };
                if validated_files.contains_key(&path) {
                    continue;
                }
                let resolved = file_selection(
                    &server,
                    &broker,
                    &path,
                    p.project_dir.as_deref(),
                    session_checkout.as_deref(),
                    &projects,
                    &checkout_rows,
                )
                .and_then(|selection| {
                    acquire_file_selection(
                        &server,
                        &broker,
                        selection,
                        CheckoutAccessKind::RenderFileProvider,
                        CheckoutAccessIntent::Read,
                    )
                });
                match resolved {
                    Ok(mut acquired) => {
                        let bytes = acquired.content.len() as u64;
                        validated_files.insert(
                            path,
                            mcp_tools::ref_size::FileInputResolution::Validated(
                                mcp_tools::ref_size::ValidatedFileInput { bytes },
                            ),
                        );
                        acquired.content = Vec::new();
                        acquired_files.push(acquired);
                    }
                    Err(error) => {
                        validated_files.insert(
                            path,
                            mcp_tools::ref_size::FileInputResolution::Rejected(error.to_string()),
                        );
                    }
                }
            }
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher);
            let output = mcp_tools::ref_size::ref_size_with_validated_files(
                &p,
                &provider_ctx,
                &validated_files,
            )?;
            for acquired in &acquired_files {
                broker
                    .revalidate(&acquired.lease)
                    .map_err(checkout_access_error)?;
            }
            drop(acquired_files);
            mcp_tools::ref_size::page_response(&p, &output)
        })
        .await
    }

    #[tool(
        name = "bbox_edge_compact",
        description = "Dry-run or apply legacy edge sidecar compaction for one project. Removes append-only derived edges from edges/<project_id>.jsonl while retaining explicit/provenance/malformed lines; apply defaults false and writes a backup before replacement. With apply=true, rebuild=true forces a sidecar-only in-memory EdgeIndex rebuild even when compaction is already complete."
    )]
    pub(crate) async fn bbox_edge_compact(
        &self,
        Parameters(p): Parameters<EdgeCompactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_edge_compact", move || {
            // §9.2 B3: raw sidecar ids stay a tagged v1 compatibility lane;
            // the catalog arm fails closed on ids the catalog does not know.
            if server.state.project_authority.is_bridge() {
                if server.resolve_project_selection(&p.project_id).is_err() {
                    server.state.resolver_compat.record(
                        "bbox_edge_compact",
                        crate::server::resolver_compat::CompatLane::RawSidecarId,
                    );
                }
            } else {
                server.validate_project_selection(&p.project_id)?;
            }
            let edges_dir = crate::server::edge_sidecar_dir(&server.state);
            let apply = p.apply.unwrap_or(false);
            let stats = edge_index::compact_legacy_sidecar(&edges_dir, &p.project_id, apply)?;
            let edge_index_rebuilt = apply && p.rebuild.unwrap_or(false);
            let mut receipt = json!({"status":"ok", "stats":stats,
                "compaction_completed":true, "edge_index_rebuilt":false});
            if edge_index_rebuilt {
                match crate::server::rebuild_edge_index_from_shared(&server.state, false) {
                    Ok(()) => receipt["edge_index_rebuilt"] = json!(true),
                    Err(_) => {
                        receipt["status"] = json!("partial");
                        receipt["error"] = json!("Compaction completed, but the in-memory edge index rebuild failed. Retry apply=true,rebuild=true to rebuild from the compacted sidecar.");
                    }
                }
            }
            Ok(receipt.to_string())
        })
        .await
    }

    #[tool(
        name = "bbox_blame",
        description = "Walk back from a code line to the conversation that produced it. Two modes: 1. Anchor-matching: the line's git blame commit matches a bbox-tracked tool-call anchor, returning the full session/brofile/arc/trigger chain. 2. Git-only fallback: no bbox anchor matches, returning git blame author info only, marked as non-bbox. Use this when you want to understand WHY a line exists, not just WHO wrote it."
    )]
    pub(crate) async fn bbox_blame(
        &self,
        Parameters(p): Parameters<BlameParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_blame", move || {
            let read_view = server.state.complete_code_read_view()?;
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher);
            let projects = server.state.records_provider.records_snapshot().records;
            let target = match mcp_tools::blame::target_identity(&p, &provider_ctx) {
                Ok(target) => target,
                Err(error) => return Ok(mcp_tools::blame::bad_input(error.to_string())),
            };

            match p.locality.clone() {
                Some(mcp_tools::blame::BlameLocalityRequestV1::Plan) => {
                    let plan = workspace_blame_plan(&server, &target, &read_view.git_overlays)?;
                    return Ok(serde_json::to_string_pretty(&json!({
                        "status": "blame_locality_plan",
                        "plan": plan,
                    }))?);
                }
                Some(mcp_tools::blame::BlameLocalityRequestV1::Resolve { plan, fact }) => {
                    let current = workspace_blame_plan(&server, &target, &read_view.git_overlays)?;
                    if plan != current {
                        bail!(
                            "error.blame_plan_stale: corpus blame authority changed after the checkout plan was issued"
                        );
                    }
                    fact.validate_against(&current)?;
                    let result = mcp_tools::blame::enrich_fact(&fact, edge_index)?;
                    let authority = session_blame_authority(&server)?;
                    server.state.blame_locality_observations.record_completed(
                        &current.project_id,
                        authority.observation_authority,
                        blame_observation_target(&current),
                    )?;
                    return Ok(result);
                }
                Some(mcp_tools::blame::BlameLocalityRequestV1::Compare {
                    plan,
                    fact,
                    legacy_response_sha256,
                }) => {
                    let current = workspace_blame_plan(&server, &target, &read_view.git_overlays)?;
                    if plan != current {
                        bail!(
                            "error.blame_plan_stale: corpus blame authority changed after the checkout plan was issued"
                        );
                    }
                    fact.validate_against(&current)?;
                    let result = mcp_tools::blame::enrich_fact(&fact, edge_index)?;
                    let canonical_result: serde_json::Value = serde_json::from_str(&result)?;
                    let local_response_sha256 = hex::encode(Sha256::digest(
                        serde_json::to_vec(&canonical_result)?,
                    ));
                    server.state.blame_locality_observations.record_comparison(
                        &current.project_id,
                        blame_observation_target(&current),
                        &local_response_sha256,
                        &legacy_response_sha256,
                    )?;
                    if local_response_sha256 != legacy_response_sha256 {
                        bail!(
                            "error.blame_locality_mismatch: checkout-local and legacy blame responses differ"
                        );
                    }
                    return Ok(result);
                }
                None
                    if server.authoritative_session_workspace_binding().is_some()
                        || server.authoritative_operator_blame_binding().is_some() =>
                {
                    bail!(
                        "error.blame_locality_required: a workspace-bound blame must execute in its checkout owner"
                    );
                }
                None => {}
            }

            enforce_blame_locality_cutover(&server, &target)?;
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let acquired = match target {
                mcp_tools::blame::BlameTargetIdentity::ProjectFile {
                    project_id,
                    indexed_path_hint,
                    line,
                    byte_offset,
                } => {
                    validate_explicit_project_selection(&server, &project_id)?;
                    let acquired = acquire_project_file(
                        &server,
                        &broker,
                        &project_id,
                        &indexed_path_hint,
                        &projects,
                    )?;
                    // Catalog corpus identity is answered AT the snapshot
                    // commit. The bridge has no overlay lane at all
                    // (`read_git_overlays_for_view` returns an empty map there
                    // by contract), so requiring evidence on that arm would
                    // change bridge output, which section 11 forbids; the
                    // bridge keeps its current-checkout behavior verbatim.
                    let source = if server.state.project_authority.is_bridge() {
                        mcp_tools::blame::BlameSource::WorkingTree {
                            content: acquired.content.clone(),
                        }
                    } else {
                        mcp_tools::blame::BlameSource::Snapshot {
                            commit: snapshot_commit_for_blame(
                                &read_view.git_overlays,
                                &project_id,
                                acquired.lease.checkout_root(),
                            )?,
                        }
                    };
                    (acquired, line, Some(byte_offset), source)
                }
                mcp_tools::blame::BlameTargetIdentity::File { input_path, line } => {
                    let checkout_rows = server.state.checkout_registry.read().rows().to_vec();
                    let selection = file_selection(
                        &server,
                        &broker,
                        &input_path,
                        None,
                        server.authoritative_session_checkout().as_deref(),
                        &projects,
                        &checkout_rows,
                    )?;
                    let acquired = acquire_file_selection(
                        &server,
                        &broker,
                        selection,
                        CheckoutAccessKind::Blame,
                        CheckoutAccessIntent::Read,
                    )?;
                    // A blame the caller addressed by PATH names no corpus
                    // snapshot, so current history is the only history it
                    // could mean. The fix condition preserves this arm.
                    let source = mcp_tools::blame::BlameSource::WorkingTree {
                        content: acquired.content.clone(),
                    };
                    (acquired, Some(line), None, source)
                }
            };
            let (acquired, line, byte_offset, source) = acquired;
            let project_rel = acquired
                .lease
                .project_root()
                .strip_prefix(acquired.lease.checkout_root())
                .map_err(|_| {
                    anyhow!("error.checkout_scope_invalid: project root escaped checkout root")
                })?;
            let git_relative_path = project_rel.join(&acquired.relative_path);
            let output = mcp_tools::blame::blame(
                mcp_tools::blame::ValidatedBlameTarget {
                    git_root: acquired.lease.checkout_root().to_path_buf(),
                    git_relative_path,
                    display_path: acquired.relative_path.clone(),
                    line,
                    byte_offset,
                    source,
                },
                edge_index,
            )?;
            broker
                .revalidate(&acquired.lease)
                .map_err(checkout_access_error)?;
            Ok(output)
        })
        .await
    }

    #[tool(
        name = "bbox_provenance_export",
        description = "Legacy overlap adapter that writes bbox provenance Git notes from blackboxd. Prefer bro provenance export for checkout-local application; retain this tool when one call must cover all registered projects."
    )]
    pub(crate) async fn bbox_provenance_export(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_provenance_export", move || {
            let read_view = server.state.complete_code_read_view()?;
            if let Some(project_id) = p.project_id.as_deref() {
                validate_explicit_project_selection(&server, project_id)?;
            }
            let projects = server.state.records_provider.records_snapshot().records;
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let (leases, inputs) = acquire_provenance_projects(
                &server,
                &broker,
                &p,
                &projects,
                CheckoutAccessIntent::Write,
            )?;
            let output = if leases.is_empty() {
                mcp_tools::provenance::export_provenance(read_view.edge_index.as_ref(), &inputs)?
            } else {
                let publication = broker
                    .publication_guard_for(leases.iter())
                    .map_err(checkout_access_error)?;
                let output = mcp_tools::provenance::export_provenance(
                    read_view.edge_index.as_ref(),
                    &inputs,
                )?;
                drop(publication);
                output
            };
            Ok(output)
        })
        .await
    }

    #[tool(
        name = "bbox_provenance_export_plan",
        description = "Return one deterministic, generation-bound provenance-note page for this MCP session's authoritative checkout. Project selection comes only from session context; callers may pass only cursor and generation pagination controls. Used by bro provenance export so Git-note writes stay checkout-local."
    )]
    pub(crate) async fn bbox_provenance_export_plan(
        &self,
        Parameters(p): Parameters<ProvenanceExportPlanParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_provenance_export_plan", move || {
            let read_view = server.state.complete_code_read_view()?;
            let (project_id, published_scope) = if let Some(checkout) =
                server.authoritative_session_checkout()
            {
                (checkout.project_id.clone(), checkout.published_scope.clone())
            } else if let Some(grant) = server.authoritative_operator_provenance_binding() {
                (grant.project_id.clone(), grant.scope.clone())
            } else {
                anyhow::bail!(
                    "error.no_authoritative_checkout: initialize MCP with authenticated provenance checkout authority"
                );
            };
            if project_id.trim().is_empty()
                || published_scope.repo_id().trim().is_empty()
                || published_scope.bbox_root_relpath().trim().is_empty()
            {
                anyhow::bail!(
                    "error.invalid_checkout_scope: authoritative checkout has no durable project scope"
                );
            }
            // Plan section 4.20: the plan stays pure corpus computation. It
            // opens no Git notes, so it takes no `ProvenanceNoteIo` lease and,
            // in catalog mode, asks only whether the catalog knows the
            // project. The attached-row membership check is a version-1
            // question: it would refuse a perfectly publishable remote-only
            // catalog project for lacking a compatibility row.
            if server.state.project_authority.is_bridge() {
                let projects = server.state.records_provider.records_snapshot().records;
                if !projects
                    .iter()
                    .any(|project| project.project_id == project_id)
                {
                    anyhow::bail!(
                        "error.project_not_registered: authoritative checkout project is absent from the registry"
                    );
                }
            } else {
                server.validate_project_selection(&project_id)?;
            }
            let notes_ref = git::notes_ref("provenance")?;
            let page = mcp_tools::provenance_plan::export_plan_page(
                &p,
                published_scope,
                &project_id,
                &notes_ref,
                read_view.edge_index.as_ref(),
            )?;
            Ok(serde_json::to_string(&page)?)
        })
        .await
    }

    #[tool(
        name = "bbox_provenance_import",
        description = "Read bbox provenance git notes and replay them into the local EdgeIndex sidecar."
    )]
    pub(crate) async fn bbox_provenance_import(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_provenance_import", move || {
            if let Some(project_id) = p.project_id.as_deref() {
                validate_explicit_project_selection(&server, project_id)?;
            }
            let projects = server.state.records_provider.records_snapshot().records;
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let (leases, inputs) = acquire_provenance_projects(
                &server,
                &broker,
                &p,
                &projects,
                CheckoutAccessIntent::Read,
            )?;
            let edges_dir = crate::server::edge_sidecar_dir(&server.state);
            let authorized_projects = leased_provenance_projects(&inputs);
            let resolve_legacy_target =
                |project_id: &str,
                 root: &Path,
                 absolute_path: &Path,
                 byte_range: Option<(u64, u64)>| {
                    authorize_legacy_provenance_target(&authorized_projects, project_id)?;
                    bbox_indexing::index::resolve_current_project_chunk_entity(
                        project_id,
                        root,
                        absolute_path,
                        byte_range,
                    )
                };
            let prepared =
                mcp_tools::provenance::prepare_provenance_import(&inputs, &resolve_legacy_target)?;
            let edges_imported = if leases.is_empty() {
                0
            } else {
                let publication = broker
                    .publication_guard_for(leases.iter())
                    .map_err(checkout_access_error)?;
                let imported = mcp_tools::provenance::publish_prepared_provenance_import(
                    prepared, &edges_dir,
                )?;
                server.state.nudge_edge_index_rebuild();
                drop(publication);
                imported
            };
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "edges_imported": edges_imported,
                "notes_ref": git::notes_ref("provenance")?,
            }))?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn compaction_receipt_survives_later_rebuild_failure() {
        let mut env = crate::util::TestEnvGuard::new();
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root.join("bro"))));
        let edges_dir = crate::server::edge_sidecar_dir(&server.state);
        std::fs::create_dir_all(&edges_dir).unwrap();
        let edge = edge_index::Edge {
            source: entity_ref::EntityRef::Knowledge { id: "first".into() },
            target: entity_ref::EntityRef::Knowledge {
                id: "second".into(),
            },
            kind: "RELATED_TO".into(),
            provenance: bbox_chunker::EdgeProvenance::Derived,
            confidence: bbox_chunker::EdgeConfidence::Exact,
            metadata: Default::default(),
            project_id: None,
        };
        let original = format!(
            "{}\nmalformed-but-preserved\n",
            serde_json::to_string(&edge).unwrap()
        );
        std::fs::write(edges_dir.join("synthetic.jsonl"), &original).unwrap();
        bbox_edge_sidecar::snapshot::switch_to_clean_snapshot(
            &edges_dir,
            "active",
            "synthetic-repo",
            Some("main"),
            "head",
            vec![edge],
            vec![],
            vec![],
        )
        .unwrap();
        env.set("BLACKBOX_EDGE_INDEX_REBUILD_MAX_INPUT_BYTES", "1");
        let result = server
            .bbox_edge_compact(Parameters(EdgeCompactParams {
                project_id: "synthetic".into(),
                apply: Some(true),
                rebuild: Some(true),
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let value: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(value["status"], "partial");
        assert_eq!(value["edge_index_rebuilt"], false);
        assert_eq!(value["stats"]["applied"], true);
        assert_eq!(
            std::fs::read_to_string(edges_dir.join("synthetic.jsonl")).unwrap(),
            "malformed-but-preserved\n"
        );
        let backup = PathBuf::from(value["stats"]["backup_path"].as_str().unwrap());
        assert!(backup.starts_with(&root));
        assert_eq!(std::fs::read_to_string(backup).unwrap(), original);
    }

    #[tokio::test]
    async fn validation_variant_pages_reconstruct_and_refuse_changed_inventory() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project = install_published_entries(&server, &root, |project| {
            vec![graph_entry(synthetic_graph(
                project,
                "shared",
                "vary",
                "vary:Node",
                "one",
            ))]
        });
        for id in 0..25 {
            install_overlay(
                &server,
                &project,
                &format!("{id:032x}"),
                synthetic_graph(&project, "shared", "vary", "vary:Node", "one"),
            );
        }
        let args = json!({"project":project,"graph_id":"shared","provisional":"all"});
        let first = server
            .bbox_project_graph_validate(Parameters(serde_json::from_value(args.clone()).unwrap()))
            .await;
        let first: serde_json::Value = serde_json::from_str(&extract_text(&first)).unwrap();
        assert_eq!(first["total"], 26);
        assert_eq!(first["count"], 20);
        let mut next = args.clone();
        next["variant_offset"] = first["next_offset"].clone();
        let unstamped = server
            .bbox_project_graph_validate(Parameters(serde_json::from_value(next.clone()).unwrap()))
            .await;
        assert_eq!(unstamped.is_error, Some(true));
        next["expected_view_stamp"] = first["view_stamp"].clone();
        let second = server
            .bbox_project_graph_validate(Parameters(serde_json::from_value(next.clone()).unwrap()))
            .await;
        let second: serde_json::Value = serde_json::from_str(&extract_text(&second)).unwrap();
        assert_eq!(second["count"], 6);
        assert!(second["next_offset"].is_null());
        assert_eq!(first["view_stamp"], second["view_stamp"]);
        install_overlay(
            &server,
            &project,
            &format!("{:032x}", 26),
            synthetic_graph(&project, "shared", "vary", "vary:Node", "two"),
        );
        let stale = server
            .bbox_project_graph_validate(Parameters(serde_json::from_value(next).unwrap()))
            .await;
        assert!(extract_text(&stale).contains("error.graph_view_changed"));
    }

    #[test]
    fn schema_mode_rejects_unknown_values_even_with_explicit_agent_flag() {
        for include_agents in [None, Some(true), Some(false)] {
            let params = DescribeSchemaParams {
                mode: Some("ful".into()),
                include_agents,
                ..Default::default()
            };
            assert!(params.include_agents_resolved().is_err());
        }
        assert!(
            !DescribeSchemaParams::default()
                .include_agents_resolved()
                .unwrap()
        );
        assert!(
            DescribeSchemaParams {
                mode: Some("full".into()),
                include_agents: None,
                ..Default::default()
            }
            .include_agents_resolved()
            .unwrap()
        );
    }
    use crate::artifacts;
    use crate::server::state::SharedState;
    use bbox_indexing::checkout_access::{
        CheckoutAccessAuthority, CheckoutAccessCandidate, CheckoutAccessError,
        CheckoutAccessObservations, CheckoutAttachmentStatus, DenyCheckoutAccess,
    };
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// Parses the governance-record fixture into a graph generation. Callable
    /// more than once per test so a provisional overlay can carry its own
    /// (identical) generation alongside the published one.
    fn load_governance_generation(
        project_id: &str,
        root: &std::path::Path,
    ) -> bbox_project_graph::GraphGeneration {
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            "governance-record",
            bbox_project_graph::GraphDocumentBytes {
                descriptor: Some(include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/graph.json"
                )),
                schema: include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/schema.json"
                ),
                vertices: include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/vertices.jsonl"
                ),
                edges: include_bytes!(
                    "../../crates/bbox-project-graph/tests/fixtures/governance-record/edges.jsonl"
                ),
            },
            bbox_project_graph::GraphParseLimits::default(),
            root.to_path_buf(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        loaded.generation.unwrap()
    }

    fn install_governance_graph(server: &BlackboxServer, root: &std::path::Path) -> String {
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(root)
            .unwrap();
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project.project_id.clone())
                .unwrap();
        let graph_id = "governance-record";
        let graph = load_governance_generation(project_id.as_str(), root);
        let scope = PublishedScope::try_new("repo-governance", ".").unwrap();
        server.state.project_graph_views.write().install_published(
            bbox_indexing::project_graph_view::PublishedProjectGraphView {
                project_id,
                scope,
                accepted_generation: "test-accepted-generation".into(),
                graphs: std::collections::BTreeMap::from([(
                    graph_id.to_string(),
                    bbox_indexing::project_graph_view::ProjectGraphViewEntry::valid(
                        graph_id.to_string(),
                        bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                            accepted_generation: "generation-one".into(),
                            accepted_commit: "a".repeat(40),
                            source_generation: None,
                            workspace_id: None,
                            content_hash: graph.fingerprint.clone(),
                        },
                        graph,
                    ),
                )]),
                evidence: bbox_project_graph::EvidenceBindingSet::default(),
            },
        );
        project.project_id
    }

    /// Builds one small synthetic graph from inline bytes. The evidence exit
    /// gate needs TWO graphs in one project, and the governance fixture is a
    /// single graph, so the second plane is authored here rather than
    /// duplicating a fixture tree on disk.
    fn synthetic_graph(
        project_id: &str,
        graph_id: &str,
        namespace: &str,
        vertex_type: &str,
        vertex_id: &str,
    ) -> bbox_project_graph::GraphGeneration {
        synthetic_graph_with_policy(
            project_id,
            graph_id,
            namespace,
            vertex_type,
            vertex_id,
            json!({}),
        )
    }

    /// Same inline single-vertex graph, with a schema-level index policy. The
    /// retrieval gate fixtures flip `text_retrieval_enabled` on the second
    /// lane without duplicating the whole binding fixture.
    fn synthetic_graph_with_policy(
        project_id: &str,
        graph_id: &str,
        namespace: &str,
        vertex_type: &str,
        vertex_id: &str,
        index_policy: serde_json::Value,
    ) -> bbox_project_graph::GraphGeneration {
        let schema = serde_json::to_vec(&json!({
            "version": 1,
            "namespace": namespace,
            "vertex_types": { vertex_type: {"properties": {"name": "string"}} },
            "edge_types": []
        }))
        .unwrap();
        let vertices = serde_json::to_vec(&json!({
            "id": vertex_id,
            "type": vertex_type,
            "label": vertex_id,
            "properties": {"name": vertex_id}
        }))
        .unwrap();
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            graph_id,
            bbox_project_graph::GraphDocumentBytes {
                descriptor: None,
                schema: &schema,
                vertices: &vertices,
                edges: b"",
            },
            bbox_project_graph::GraphParseLimits::default(),
            std::path::PathBuf::new(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        let mut generation = loaded.generation.unwrap();
        if index_policy
            .as_object()
            .is_some_and(|policy| !policy.is_empty())
        {
            generation.schema.index_policy =
                serde_json::from_value(index_policy).expect("test policy block is valid");
        }
        generation
    }

    /// One authored graph with a hub vertex and `leaves` leaf vertices, joined
    /// by one edge type. The fan-out exit gate needs a neighborhood wider
    /// than the default per-hop cap without depending on fixture file order.
    fn hub_graph(project_id: &str, leaves: usize) -> bbox_project_graph::GraphGeneration {
        let schema = serde_json::to_vec(&json!({
            "version": 1,
            "namespace": "fan",
            "vertex_types": {
                "fan:Hub": {"properties": {"name": "string"}},
                "fan:Leaf": {"properties": {"name": "string"}}
            },
            "edge_types": [
                {
                    "type": "fan:LINKS",
                    "endpoints": [{"from": "fan:Hub", "to": "fan:Leaf"}],
                    "properties": {"note": "string"}
                }
            ]
        }))
        .unwrap();
        let mut vertices = String::new();
        vertices.push_str(
            &serde_json::to_string(&json!({
                "id": "hub",
                "type": "fan:Hub",
                "label": "hub",
                "properties": {"name": "hub"}
            }))
            .unwrap(),
        );
        vertices.push('\n');
        for idx in 1..=leaves {
            let leaf = serde_json::to_string(&json!({
                "id": format!("leaf-{idx}"),
                "type": "fan:Leaf",
                "label": format!("leaf-{idx}"),
                "properties": {"name": format!("leaf-{idx}")}
            }))
            .unwrap();
            vertices.push_str(&leaf);
            vertices.push('\n');
        }
        let mut edges = String::new();
        for idx in 1..=leaves {
            let edge = serde_json::to_string(&json!({
                "from": "hub",
                "type": "fan:LINKS",
                "to": format!("leaf-{idx}"),
                "properties": {"note": format!("leaf-{idx}")}
            }))
            .unwrap();
            edges.push_str(&edge);
            edges.push('\n');
        }
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            "fan",
            bbox_project_graph::GraphDocumentBytes {
                descriptor: None,
                schema: &schema,
                vertices: vertices.as_bytes(),
                edges: edges.as_bytes(),
            },
            bbox_project_graph::GraphParseLimits::default(),
            std::path::PathBuf::new(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        loaded.generation.unwrap()
    }

    fn graph_entry(
        graph: bbox_project_graph::GraphGeneration,
    ) -> bbox_indexing::project_graph_view::ProjectGraphViewEntry {
        let content_hash = graph.fingerprint.clone();
        let graph_id = graph.key.graph_id.clone();
        bbox_indexing::project_graph_view::ProjectGraphViewEntry::valid(
            graph_id,
            bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                accepted_generation: "generation-one".into(),
                accepted_commit: "a".repeat(40),
                source_generation: None,
                workspace_id: None,
                content_hash,
            },
            graph,
        )
    }

    /// Installs a published view whose entries are built against the freshly
    /// registered project id, so synthetic fixtures can embed it in their
    /// parsed generation keys.
    fn install_published_entries<F>(
        server: &BlackboxServer,
        root: &std::path::Path,
        build: F,
    ) -> String
    where
        F: FnOnce(&str) -> Vec<bbox_indexing::project_graph_view::ProjectGraphViewEntry>,
    {
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(root)
            .unwrap();
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project.project_id.clone())
                .unwrap();
        let graphs = build(project_id.as_str())
            .into_iter()
            .map(|entry| (entry.graph_id.clone(), entry))
            .collect::<std::collections::BTreeMap<_, _>>();
        server.state.project_graph_views.write().install_published(
            bbox_indexing::project_graph_view::PublishedProjectGraphView {
                project_id,
                scope: PublishedScope::try_new("test-plane", ".").unwrap(),
                accepted_generation: "test-accepted-generation".into(),
                graphs,
                evidence: bbox_project_graph::EvidenceBindingSet::default(),
            },
        );
        project.project_id
    }

    /// One valid graph whose schema serializes well past one 4 KiB body page,
    /// with escaped multibyte characters in the property payloads. The
    /// exact-read contract tests page it instead of depending on the
    /// incidental size of a fixture file.
    fn wide_schema_graph(
        project_id: &str,
        type_count: usize,
    ) -> bbox_project_graph::GraphGeneration {
        let mut vertex_types = serde_json::Map::new();
        for idx in 0..type_count {
            vertex_types.insert(
                format!("wide:Type{idx:02}"),
                json!({
                    "properties": {
                        "name": "string",
                        "notes": { format!("note-{idx:02}-🦀-日本語-{}", "x".repeat(48)): "string" }
                    }
                }),
            );
        }
        let schema = serde_json::to_vec(&json!({
            "version": 1,
            "namespace": "wide",
            "vertex_types": vertex_types,
            "edge_types": [
                {
                    "type": "wide:LINKS",
                    "endpoints": [{"from": "wide:Type00", "to": "wide:Type00"}],
                    "properties": {"note": "string"}
                }
            ]
        }))
        .unwrap();
        let vertices = serde_json::to_vec(&json!({
            "id": "seed",
            "type": "wide:Type00",
            "label": "seed",
            "properties": {"name": "seed"}
        }))
        .unwrap();
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            "wide",
            bbox_project_graph::GraphDocumentBytes {
                descriptor: None,
                schema: &schema,
                vertices: &vertices,
                edges: b"",
            },
            bbox_project_graph::GraphParseLimits::default(),
            std::path::PathBuf::new(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        loaded.generation.unwrap()
    }

    /// One valid graph whose retrieval policy excludes a large type set, so
    /// the summary's exclusion metadata is a count while the exact sorted
    /// list stays recoverable through the schema body read.
    fn exclusion_heavy_graph(
        project_id: &str,
        type_count: usize,
    ) -> bbox_project_graph::GraphGeneration {
        let excluded: Vec<String> = (0..type_count)
            .map(|idx| format!("excl:Type{idx:03}"))
            .collect();
        let mut vertex_types = serde_json::Map::new();
        for name in &excluded {
            vertex_types.insert(name.clone(), json!({"properties": {"name": "string"}}));
        }
        let schema = serde_json::to_vec(&json!({
            "version": 1,
            "namespace": "excl",
            "vertex_types": vertex_types,
            "edge_types": [],
            "index_policy": {"retrieval_excluded_types": excluded}
        }))
        .unwrap();
        let vertices = serde_json::to_vec(&json!({
            "id": "seed",
            "type": "excl:Type000",
            "label": "seed",
            "properties": {"name": "seed"}
        }))
        .unwrap();
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            "heavy",
            bbox_project_graph::GraphDocumentBytes {
                descriptor: None,
                schema: &schema,
                vertices: &vertices,
                edges: b"",
            },
            bbox_project_graph::GraphParseLimits::default(),
            std::path::PathBuf::new(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        loaded.generation.unwrap()
    }

    /// The synthetic validation-error fixture: `count` distinct rows with
    /// multibyte characters, so paging and exact recovery must respect UTF-8
    /// byte boundaries rather than slicing mid-character.
    fn synthetic_validation_errors(count: usize) -> Vec<bbox_project_graph::ValidationError> {
        (0..count)
            .map(|idx| {
                bbox_project_graph::ValidationError::new(
                    "edge.missing_vertex",
                    "edges.jsonl",
                    Some(idx + 1),
                    format!("edge target vertex-{idx:03} is missing 🦀 缺口 {idx:04}"),
                )
            })
            .collect()
    }

    fn invalid_graph_entry(
        graph_id: &str,
        error_count: usize,
    ) -> bbox_indexing::project_graph_view::ProjectGraphViewEntry {
        bbox_indexing::project_graph_view::ProjectGraphViewEntry::invalid(
            graph_id.to_string(),
            bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                accepted_generation: "generation-one".into(),
                accepted_commit: "b".repeat(40),
                source_generation: None,
                workspace_id: None,
                content_hash: format!("invalid-{error_count:04}"),
            },
            synthetic_validation_errors(error_count),
        )
    }

    /// Installs the exit-gate shape: a tenant record graph, a separate source
    /// graph in the SAME project, and a binding set joining a record vertex to
    /// a source vertex and that source vertex to a published project file.
    ///
    /// Two project-authored graphs stand in for the record/source split. The
    /// connector-managed source graph belongs to the sibling milestone, and
    /// the cross-plane variant becomes a follow-up test once both branches
    /// fold; the binding layer cannot tell the difference, because it
    /// addresses both planes by canonical vertex ref.
    fn install_evidence_fixture(
        server: &BlackboxServer,
        root: &std::path::Path,
    ) -> (String, String) {
        let project_id_raw = {
            let project = server
                .state
                .project_authority
                .bridge_registry()
                .unwrap()
                .write()
                .register_path(root)
                .unwrap();
            project.project_id
        };
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project_id_raw).unwrap();
        let source = synthetic_graph(
            project_id.as_str(),
            "source",
            "dataset",
            "dataset:Asset",
            "asset-1",
        );
        install_bound_pair(server, root, project_id, source)
    }

    /// The M9a retrieval-gate fixture: the same record/source binding shape,
    /// with the source lane's policy controlling text retrieval. Traversal
    /// must refuse the hop when the policy is off, while inspection keeps
    /// showing the binding for diagnosis (the design's deliberate
    /// asymmetry between caller-asserted bindings and unreadable graphs).
    fn install_retrieval_gated_fixture(
        server: &BlackboxServer,
        root: &std::path::Path,
        source_text_retrieval_enabled: bool,
    ) -> (String, String) {
        let project_id_raw = {
            let project = server
                .state
                .project_authority
                .bridge_registry()
                .unwrap()
                .write()
                .register_path(root)
                .unwrap();
            project.project_id
        };
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project_id_raw).unwrap();
        let source = synthetic_graph_with_policy(
            project_id.as_str(),
            "source",
            "dataset",
            "dataset:Asset",
            "asset-1",
            json!({"text_retrieval_enabled": source_text_retrieval_enabled}),
        );
        install_bound_pair(server, root, project_id, source)
    }

    /// Shared installer behind the evidence fixtures: registers the project,
    /// authors the record lane, and installs both lanes plus the binding set.
    fn install_bound_pair(
        server: &BlackboxServer,
        root: &std::path::Path,
        project_id: bbox_corpus_core::project_catalog::ProjectId,
        source: bbox_project_graph::GraphGeneration,
    ) -> (String, String) {
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(root)
            .unwrap();
        let records = synthetic_graph(
            project_id.as_str(),
            "records",
            "record",
            "record:Filing",
            "filing-1",
        );
        let document = serde_json::to_vec(&json!({
            "version": 1,
            "bindings": [
                {
                    "binding_id": "record-to-source",
                    "source": {
                        "kind": "graph_vertex",
                        "graph_id": "records",
                        "vertex_id": "filing-1"
                    },
                    "kind": "record:CORRESPONDS_TO",
                    "target": {
                        "kind": "graph_vertex",
                        "graph_id": "source",
                        "vertex_id": "asset-1"
                    },
                    "assertion_authority": "project",
                    "mapping_version": "mapping-v1",
                    "asserted_at": "2026-01-01T00:00:00Z",
                    "source_generation": 1,
                    "target_generation": 1
                },
                {
                    "binding_id": "source-to-file",
                    "source": {
                        "kind": "graph_vertex",
                        "graph_id": "source",
                        "vertex_id": "asset-1"
                    },
                    "kind": "dataset:EVIDENCED_BY",
                    "target": {
                        "kind": "project_file",
                        "rel_path_hash": "pathhash",
                        "chunk_hash": "chunkhash",
                        "occurrence_idx": 0
                    },
                    "assertion_authority": "connector",
                    "observation_id": "observation-file-1",
                    "asserted_at": "2026-01-01T00:00:00Z",
                    "source_generation": 1
                }
            ]
        }))
        .unwrap();
        let evidence = bbox_project_graph::parse_evidence_document(
            project_id.as_str(),
            &document,
            bbox_project_graph::EvidenceParseLimits::default(),
        )
        .bindings
        .expect("fixture binding document is valid");
        let scope = PublishedScope::try_new("repo-evidence", ".").unwrap();
        server.state.project_graph_views.write().install_published(
            bbox_indexing::project_graph_view::PublishedProjectGraphView {
                project_id,
                scope,
                accepted_generation: "test-accepted-generation".into(),
                graphs: std::collections::BTreeMap::from([
                    ("records".to_string(), graph_entry(records)),
                    ("source".to_string(), graph_entry(source)),
                ]),
                evidence,
            },
        );
        let file_ref = format!("project_file:{}:pathhash:chunkhash:0", project.project_id);
        (project.project_id, file_ref)
    }

    async fn inspect_published(server: &BlackboxServer, entity_ref: &str) -> serde_json::Value {
        let result = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                edge_cursor: None,
                property: None,
                property_cursor: None,
                property_limit: None,
                entity_ref: entity_ref.to_string(),
                provisional: Some("published".into()),
                edge_types: None,
                direction: Some("both".into()),
                per_type_limit: Some(10),
                property_mode: Some("full".into()),
            }))
            .await;
        serde_json::from_str(&extract_text(&result)).unwrap()
    }

    /// The JSON a serialized `EntityRef` takes on a path step.
    ///
    /// `EntityRef` is an internally tagged enum, so it rides the wire as an
    /// OBJECT keyed by `type`, not as its rendered `type:segments` string.
    /// `steps[n]["to"]` is therefore never a JSON string, and comparing it
    /// against a rendered ref silently compares an object to a string rather
    /// than failing loudly at the point of the mistake. Build the expected
    /// value from the ref itself so the assertion tracks the real wire shape.
    fn step_ref(rendered: &str) -> serde_json::Value {
        serde_json::to_value(entity_ref::EntityRef::parse(rendered).unwrap()).unwrap()
    }

    /// THE EXIT GATE for milestone 3: a tenant record vertex traverses through
    /// a source vertex to a published project file, and the reverse traversal
    /// preserves provenance.
    #[tokio::test]
    async fn a_record_vertex_traverses_through_a_source_vertex_to_a_project_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let (project_id, file_ref) = install_evidence_fixture(&server, &root);
        let record_ref = format!("project_graph_vertex:{project_id}:records:filing-1");

        let forward = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: record_ref.clone(),
                provisional: Some("published".into()),
                to: Some(file_ref.clone()),
                to_type: None,
                edge_types: None,
                max_depth: Some(3),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let forward_text = extract_text(&forward);
        let forward: serde_json::Value = serde_json::from_str(&forward_text).unwrap();
        let paths = forward["paths"].as_array().expect("paths array");
        assert!(
            !paths.is_empty(),
            "record vertex must reach the project file: {forward_text}"
        );
        let steps = paths[0]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2, "{forward_text}");
        // Hop one crosses from the record graph into a DIFFERENT graph of the
        // same project, which is what the cross-graph namespace split needed.
        assert_eq!(steps[0]["edge_kind"], "record:CORRESPONDS_TO");
        assert_eq!(
            steps[0]["metadata"]["evidence.binding_id"],
            "record-to-source"
        );
        assert_eq!(
            steps[0]["metadata"]["evidence.mapping_version"],
            "mapping-v1"
        );
        assert_eq!(
            steps[0]["to"],
            step_ref(&format!("project_graph_vertex:{project_id}:source:asset-1")),
            "{forward_text}"
        );
        // Hop two leaves the graph plane entirely for a project file ref.
        assert_eq!(steps[1]["edge_kind"], "dataset:EVIDENCED_BY");
        assert_eq!(
            steps[1]["metadata"]["evidence.observation_id"],
            "observation-file-1"
        );
        assert_eq!(steps[1]["to"], step_ref(&file_ref), "{forward_text}");

        // The reverse traversal preserves provenance: same bindings, same
        // authority and observation labels, walked from the file back.
        let reverse = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: file_ref.clone(),
                provisional: Some("published".into()),
                to: Some(record_ref),
                to_type: None,
                edge_types: None,
                max_depth: Some(3),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let reverse_text = extract_text(&reverse);
        let reverse: serde_json::Value = serde_json::from_str(&reverse_text).unwrap();
        let paths = reverse["paths"].as_array().expect("paths array");
        assert!(
            !paths.is_empty(),
            "the project file must reach back to the record vertex: {reverse_text}"
        );
        let steps = paths[0]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 2, "{reverse_text}");
        // Walking back: file -> source vertex -> record vertex. A step records
        // `from`/`to` in walk order and labels the edge direction separately,
        // so both hops are `in` while the refs advance toward the record.
        assert_eq!(steps[0]["direction"], "in");
        assert_eq!(steps[0]["from"], step_ref(&file_ref), "{reverse_text}");
        assert_eq!(
            steps[0]["to"],
            step_ref(&format!("project_graph_vertex:{project_id}:source:asset-1")),
            "{reverse_text}"
        );
        assert_eq!(
            steps[0]["metadata"]["evidence.observation_id"], "observation-file-1",
            "reverse traversal must carry the same observation provenance"
        );
        assert_eq!(
            steps[0]["metadata"]["evidence.assertion_authority"],
            "connector"
        );
        assert_eq!(steps[1]["direction"], "in");
        assert_eq!(
            steps[1]["to"],
            step_ref(&format!(
                "project_graph_vertex:{project_id}:records:filing-1"
            )),
            "{reverse_text}"
        );
        assert_eq!(
            steps[1]["metadata"]["evidence.assertion_authority"],
            "project"
        );
        assert_eq!(
            steps[1]["metadata"]["evidence.mapping_version"],
            "mapping-v1"
        );
    }

    /// THE EXIT GATE for M9a (c): a traversal that would cross into a graph
    /// whose policy disables text retrieval does not enumerate that graph's
    /// vertices. The binding still exists and inspection still shows it (the
    /// deliberate asymmetry: a binding is the caller's own assertion), but
    /// the walk refuses the hop: no path, no truncated-expansion note, and no
    /// rendered mention that could imply the excluded lane exists.
    #[tokio::test]
    async fn traversal_does_not_cross_into_a_retrieval_disabled_graph() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let (project_id, file_ref) = install_retrieval_gated_fixture(&server, &root, false);
        let record_ref = format!("project_graph_vertex:{project_id}:records:filing-1");

        // The walk toward the file must cross the disabled lane; it refuses
        // at the hop instead of returning a truncated path.
        let blocked = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: record_ref.clone(),
                provisional: Some("published".into()),
                to: Some(file_ref.clone()),
                to_type: None,
                edge_types: None,
                max_depth: Some(3),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let blocked_text = extract_text(&blocked);
        assert!(
            blocked_text.contains("No paths found"),
            "the disabled lane must not be walked: {blocked_text}"
        );
        assert!(
            !blocked_text.contains("source:asset-1"),
            "the refused hop must not disclose the excluded vertex: {blocked_text}"
        );
        let blocked_value: serde_json::Value = serde_json::from_str(&blocked_text).unwrap();
        assert!(
            blocked_value["truncated_expansions"]
                .as_array()
                .is_none_or(std::vec::Vec::is_empty),
            "no count or note may imply the excluded lane exists: {blocked_text}"
        );

        // An open-ended walk to the nearest graph vertices refuses the same
        // hop; the record vertex itself is the root, not a found path.
        let open = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: record_ref.clone(),
                provisional: Some("published".into()),
                to: None,
                to_type: Some("project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(2),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let open_text = extract_text(&open);
        let open: serde_json::Value = serde_json::from_str(&open_text).unwrap();
        // The record vertex's own reflected meta:INSTANCE_OF hop stays
        // walkable (it lives in the readable lane); the disabled lane does
        // not. No path may contain a vertex of the source graph.
        for path in open["paths"].as_array().unwrap() {
            for step in path["steps"].as_array().unwrap() {
                let endpoint = step["to"].as_object().unwrap();
                assert_ne!(
                    endpoint.get("graph_id").and_then(|value| value.as_str()),
                    Some("source"),
                    "the disabled lane must not enter the frontier: {open_text}"
                );
            }
        }
        assert!(
            !open_text.contains("source:asset-1"),
            "the refused hop must not disclose the excluded vertex: {open_text}"
        );

        // Inspection keeps the binding: it is the caller's own assertion,
        // retained for diagnosis exactly like an unauthorized endpoint.
        let inspected = inspect_published(&server, &record_ref).await;
        let binding = inspected["edges"]["out"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["kind"] == "record:CORRESPONDS_TO")
            .unwrap_or_else(|| panic!("inspection retains the binding: {inspected}"));
        assert_eq!(
            binding["properties"]["evidence.binding_id"],
            json!("record-to-source"),
            "{inspected}"
        );

        // The describe surface explains WHY the lane is absent from search:
        // policy flags, counts, and both generations in one place.
        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project_id.clone(),
                graph_id: "source".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let described_text = extract_text(&described);
        assert!(
            described_text.contains("\"text_retrieval_enabled\": false"),
            "{described_text}"
        );
        assert!(
            described_text.contains("\"indexable\": false"),
            "{described_text}"
        );
        assert!(
            described_text.contains("\"indexed_vertex_count\": 0"),
            "{described_text}"
        );
        assert!(
            described_text.contains("\"embedded_vertex_count\": 0"),
            "{described_text}"
        );

        // Control: the identical fixture with retrieval enabled walks the
        // same two hops, proving the refusal above is the policy gate and
        // not the fixture shape.
        let control_tmp = tempfile::tempdir().unwrap();
        let control_root = control_tmp.path().canonicalize().unwrap();
        let control_server = test_server(&control_tmp);
        let (control_project, control_file) =
            install_retrieval_gated_fixture(&control_server, &control_root, true);
        let forward = control_server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: format!("project_graph_vertex:{control_project}:records:filing-1"),
                provisional: Some("published".into()),
                to: Some(control_file),
                to_type: None,
                edge_types: None,
                max_depth: Some(3),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let forward_text = extract_text(&forward);
        assert!(
            !forward_text.contains("No paths found"),
            "the enabled lane must be walkable: {forward_text}"
        );
    }

    /// THE EXIT GATE for M9a (fan-out cap): a hub wider than the default
    /// per-hop cap is expanded to the cap only, and the response says so
    /// explicitly in both the structured field and the rendered text.
    #[tokio::test]
    async fn find_paths_caps_fanout_and_reports_the_truncation_at_the_tool_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&root)
            .unwrap();
        let project_id =
            bbox_corpus_core::project_catalog::ProjectId::parse(project.project_id.clone())
                .unwrap();
        let graph = hub_graph(project_id.as_str(), 20);
        let scope = PublishedScope::try_new("repo-fan", ".").unwrap();
        server.state.project_graph_views.write().install_published(
            bbox_indexing::project_graph_view::PublishedProjectGraphView {
                project_id,
                scope,
                accepted_generation: "test-accepted-generation".into(),
                graphs: std::collections::BTreeMap::from([("fan".to_string(), graph_entry(graph))]),
                evidence: bbox_project_graph::EvidenceBindingSet::default(),
            },
        );
        let hub_ref = format!("project_graph_vertex:{}:fan:hub", project.project_id);

        let capped = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: hub_ref.clone(),
                provisional: Some("published".into()),
                to: None,
                to_type: Some("project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(1),
                limit: Some(30),
                max_fanout: None,
            }))
            .await;
        let capped_text = extract_text(&capped);
        let capped: serde_json::Value = serde_json::from_str(&capped_text).unwrap();
        let capped_paths = capped["paths"].as_array().unwrap();
        assert_eq!(
            capped_paths.len(),
            16,
            "the default cap enumerates sixteen neighbors of the hub: {capped_text}"
        );
        let truncations = capped["truncated_expansions"].as_array().unwrap();
        assert_eq!(truncations.len(), 1, "{capped_text}");
        assert_eq!(truncations[0]["vertex"], json!(hub_ref));
        assert!(
            capped_text.contains("Expansion truncated at the max_fanout cap"),
            "{capped_text}"
        );

        // Raising the cap past the neighborhood enumerates every neighbor
        // and reports no truncation at all. The reflected graph adds the
        // hub's meta:INSTANCE_OF edge to its schema-as-data type vertex, so
        // the full neighborhood is the twenty authored leaves plus one
        // reflected hop; the capped run's reported edge count must equal the
        // full run's found paths, keeping the two runs consistent.
        let full = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: hub_ref.clone(),
                provisional: Some("published".into()),
                to: None,
                to_type: Some("project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(1),
                limit: Some(30),
                max_fanout: Some(64),
            }))
            .await;
        let full_text = extract_text(&full);
        let full: serde_json::Value = serde_json::from_str(&full_text).unwrap();
        let full_paths = full["paths"].as_array().unwrap();
        assert_eq!(
            full_paths.len(),
            21,
            "a raised cap enumerates every neighbor: {full_text}"
        );
        assert_eq!(
            full["truncated_expansions"].as_array().unwrap().len(),
            0,
            "{full_text}"
        );
        assert_eq!(truncations[0]["edge_count"], json!(full_paths.len()));
    }

    /// Q10: `provisional` is the canonical spelling on the project graph
    /// family and `visibility` keeps working as a deprecated serde alias.
    #[test]
    fn project_graph_params_accept_visibility_as_a_deprecated_alias() {
        let exact: ProjectGraphDescribeParams =
            serde_json::from_str(r#"{"project":"p1","graph_id":"g","visibility":"own"}"#).unwrap();
        assert_eq!(exact.provisional.as_deref(), Some("own"));
        let listed: ProjectGraphListParams =
            serde_json::from_str(r#"{"visibility":"all"}"#).unwrap();
        assert_eq!(listed.provisional.as_deref(), Some("all"));
        let canonical: ProjectGraphListParams =
            serde_json::from_str(r#"{"provisional":"published"}"#).unwrap();
        assert_eq!(canonical.provisional.as_deref(), Some("published"));
    }

    /// Shared driver for the exact detail reads: returns the raw tool text so
    /// refusals stay assertable alongside well-formed pages.
    async fn describe_detail_text(
        server: &BlackboxServer,
        project: &str,
        graph_id: &str,
        detail: &str,
        cursor: Option<String>,
        body_limit: Option<usize>,
    ) -> String {
        let result = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project.into(),
                graph_id: graph_id.into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: Some(detail.into()),
                cursor,
                body_limit,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        extract_text(&result)
    }

    /// Walks every body page of one detail read, asserting each serialized
    /// page envelope stays inside the shared page budget, and returns the
    /// concatenated exact bytes plus the page count.
    async fn collect_describe_body(
        server: &BlackboxServer,
        project: &str,
        graph_id: &str,
        detail: &str,
        body_limit: Option<usize>,
    ) -> (String, usize) {
        let mut joined = String::new();
        let mut cursor = None;
        let mut pages = 0usize;
        loop {
            let text =
                describe_detail_text(server, project, graph_id, detail, cursor, body_limit).await;
            let page: serde_json::Value = serde_json::from_str(&text).unwrap();
            let envelope = serde_json::to_vec(&page).unwrap().len();
            assert!(
                envelope <= 6144,
                "detail pages stay inside the shared page budget: {envelope}"
            );
            assert_eq!(page["status"], json!("ok"), "{text}");
            joined.push_str(page["body"]["text"].as_str().unwrap());
            pages += 1;
            cursor = page["body"]["next_cursor"]
                .as_str()
                .map(ToString::to_string);
            if cursor.is_none() {
                return (joined, pages);
            }
        }
    }

    /// Same walk for the exact validation-error array.
    async fn collect_validation_errors(
        server: &BlackboxServer,
        project: &str,
        graph_id: &str,
        body_limit: Option<usize>,
    ) -> String {
        let mut joined = String::new();
        let mut cursor = None;
        loop {
            let result = server
                .bbox_project_graph_validate(Parameters(ProjectGraphValidateParams {
                    project: project.into(),
                    graph_id: graph_id.into(),
                    provisional: Some("published".into()),
                    source: None,
                    checkout_id: None,
                    expected_content_hash: None,
                    detail: Some("errors".into()),
                    cursor,
                    body_limit,
                    error_offset: None,
                    error_limit: None,
                    expected_error_stamp: None,
                    variant_limit: None,
                    variant_offset: None,
                    expected_view_stamp: None,
                }))
                .await;
            let text = extract_text(&result);
            let page: serde_json::Value = serde_json::from_str(&text).unwrap();
            joined.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"]
                .as_str()
                .map(ToString::to_string);
            if cursor.is_none() {
                return joined;
            }
        }
    }

    /// A04: the default describe is a compact summary. Schema identity and
    /// counts arrive inline; the schema body stays behind the exact detail
    /// read, and page reconstruction is byte-exact across escaped multibyte
    /// characters, default pages, and a tiny body_limit.
    #[tokio::test]
    async fn project_graph_describe_default_is_compact_and_schema_recovers_exactly() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            vec![graph_entry(wide_schema_graph(project, 60))]
        });
        let expected_schema =
            serde_json::to_value(&wide_schema_graph(&project_id, 60).schema).unwrap();
        let schema_bytes = serde_json::to_vec(&expected_schema).unwrap().len();
        assert!(
            schema_bytes > 8192,
            "fixture must span multiple default body pages: {schema_bytes}"
        );

        let summary = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project_id.clone(),
                graph_id: "wide".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let summary_text = extract_text(&summary);
        let summary_page: serde_json::Value = serde_json::from_str(&summary_text).unwrap();
        assert_eq!(summary_page["detail"], json!("summary"), "{summary_text}");
        assert_eq!(summary_page["summary"]["graph_id"], json!("wide"));
        assert_eq!(summary_page["summary"]["status"], json!("valid"));
        assert_eq!(summary_page["summary"]["source"], json!("published"));
        assert_eq!(summary_page["schema"]["vertex_type_count"], json!(60));
        assert_eq!(summary_page["schema"]["edge_type_count"], json!(1));
        assert!(
            summary_page["schema"]["schema_id"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        assert!(
            !summary_text.contains("vertex_types"),
            "compact summary must not embed the schema body: {summary_text}"
        );
        let summary_envelope = serde_json::to_vec(&summary_page).unwrap().len();
        assert!(
            summary_envelope < 3000,
            "serialized summary stays compact: {summary_envelope}"
        );

        let (joined, pages) =
            collect_describe_body(&server, &project_id, "wide", "schema", None).await;
        assert!(
            pages > 1,
            "one default page cannot hold {schema_bytes} bytes"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
            expected_schema
        );

        let (tiny_joined, tiny_pages) =
            collect_describe_body(&server, &project_id, "wide", "schema", Some(97)).await;
        assert!(
            tiny_pages > pages,
            "a 97-byte body_limit must split into more pages than the default"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&tiny_joined).unwrap(),
            expected_schema
        );

        let (descriptor, descriptor_pages) =
            collect_describe_body(&server, &project_id, "wide", "descriptor", None).await;
        let descriptor_value: serde_json::Value = serde_json::from_str(&descriptor).unwrap();
        assert_eq!(descriptor_value["graph_id"], json!("wide"));
        assert_eq!(
            descriptor_value["authority"],
            json!(bbox_project_graph::GraphAuthority::Project)
        );
        assert_eq!(
            descriptor_value["schema_id"],
            summary_page["schema"]["schema_id"]
        );
        assert!(
            descriptor_pages >= 1,
            "descriptor walks at least one complete page"
        );
    }

    /// A04: body cursors are content-bound. A cursor from one graph, one
    /// detail kind, or one since-replaced generation must refuse instead of
    /// silently paging different bytes.
    #[tokio::test]
    async fn project_graph_describe_body_cursors_are_content_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            vec![
                graph_entry(wide_schema_graph(project, 60)),
                graph_entry(synthetic_graph(project, "tiny", "tiny", "tiny:Node", "n1")),
            ]
        });

        let first =
            describe_detail_text(&server, &project_id, "wide", "schema", None, Some(64)).await;
        let first_page: serde_json::Value = serde_json::from_str(&first).unwrap();
        let cursor = first_page["body"]["next_cursor"]
            .as_str()
            .expect("a 64-byte page of a multi-KiB schema always continues")
            .to_string();

        let cross_graph = describe_detail_text(
            &server,
            &project_id,
            "tiny",
            "schema",
            Some(cursor.clone()),
            None,
        )
        .await;
        assert!(
            cross_graph.contains("restart without cursor"),
            "a cursor must not cross graph ids: {cross_graph}"
        );

        let cross_detail = describe_detail_text(
            &server,
            &project_id,
            "wide",
            "descriptor",
            Some(cursor.clone()),
            None,
        )
        .await;
        assert!(
            cross_detail.contains("restart without cursor"),
            "a cursor must not cross detail kinds: {cross_detail}"
        );

        // Replace the wide graph with a changed generation; the old cursor
        // names bytes that no longer exist.
        {
            let mut views = server.state.project_graph_views.write();
            let parsed =
                bbox_corpus_core::project_catalog::ProjectId::parse(project_id.clone()).unwrap();
            let mut view = views.published_view(&parsed).unwrap().clone();
            view.graphs.insert(
                "wide".into(),
                graph_entry(wide_schema_graph(project_id.as_str(), 61)),
            );
            views.install_published(view);
        }
        let stale = describe_detail_text(
            &server,
            &project_id,
            "wide",
            "schema",
            Some(cursor.clone()),
            None,
        )
        .await;
        assert!(
            stale.contains("restart without cursor"),
            "a cursor must refuse a replaced generation: {stale}"
        );

        let refused = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project_id.clone(),
                graph_id: "wide".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: Some(cursor),
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let refused_text = extract_text(&refused);
        assert!(
            refused_text.contains("error.bad_input"),
            "the compact summary takes no cursor: {refused_text}"
        );

        let unknown =
            describe_detail_text(&server, &project_id, "wide", "museum", None, None).await;
        assert!(
            unknown.contains("error.bad_input"),
            "unknown detail values refuse: {unknown}"
        );
    }

    /// Installs one provisional overlay for a distinct checkout, standing in
    /// for a second workspace's uncommitted variant of the same graph id.
    fn install_overlay(
        server: &BlackboxServer,
        project_id: &str,
        workspace_hex: &str,
        graph: bbox_project_graph::GraphGeneration,
    ) -> String {
        let workspace_id = bro_core::WorkspaceId::parse(workspace_hex.to_string()).unwrap();
        let graph_id = graph.key.graph_id.clone();
        server
            .state
            .project_graph_views
            .write()
            .install_provisional(
                bbox_indexing::project_graph_view::ProvisionalProjectGraphOverlay {
                    project_id: bbox_corpus_core::project_catalog::ProjectId::parse(
                        project_id.to_string(),
                    )
                    .unwrap(),
                    scope: PublishedScope::try_new("test-plane", ".").unwrap(),
                    workspace_id: workspace_id.clone(),
                    source_generation_id: "working-one".into(),
                    graphs: std::collections::BTreeMap::from([(
                        graph_id.clone(),
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(
                            bbox_indexing::project_graph_view::ProjectGraphViewEntry::valid(
                                graph_id,
                                bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                                    accepted_generation: "generation-one".into(),
                                    accepted_commit: "a".repeat(40),
                                    source_generation: Some("working-one".into()),
                                    workspace_id: Some(workspace_id),
                                    content_hash: graph.fingerprint.clone(),
                                },
                                graph,
                            ),
                        ),
                    )]),
                    evidence: None,
                },
            );
        workspace_hex.to_string()
    }

    /// Detail read with the precise variant selector. The selector mirrors
    /// the list entry identity: authority plane, checkout, and content hash.
    async fn describe_selected_text(
        server: &BlackboxServer,
        project: &str,
        graph_id: &str,
        detail: &str,
        source: Option<&str>,
        checkout_id: Option<&str>,
        expected_content_hash: Option<&str>,
        cursor: Option<String>,
        body_limit: Option<usize>,
    ) -> String {
        let result = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project.into(),
                graph_id: graph_id.into(),
                provisional: Some("all".into()),
                source: source.map(Into::into),
                checkout_id: checkout_id.map(Into::into),
                expected_content_hash: expected_content_hash.map(Into::into),
                detail: Some(detail.into()),
                cursor,
                body_limit,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        extract_text(&result)
    }

    /// Multi-variant summary page driver with the same selector fields.
    async fn describe_variants_text(
        server: &BlackboxServer,
        project: &str,
        graph_id: &str,
        source: Option<&str>,
        checkout_id: Option<&str>,
        expected_content_hash: Option<&str>,
        variant_limit: Option<usize>,
        variant_offset: Option<usize>,
        expected_view_stamp: Option<String>,
    ) -> String {
        let result = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project.into(),
                graph_id: graph_id.into(),
                provisional: Some("all".into()),
                source: source.map(Into::into),
                checkout_id: checkout_id.map(Into::into),
                expected_content_hash: expected_content_hash.map(Into::into),
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit,
                variant_offset,
                expected_view_stamp,
            }))
            .await;
        extract_text(&result)
    }

    /// A04 follow-up: one graph id can be visible as several variants at
    /// once, and distinct sources/checkouts can repeat one content hash. The
    /// summary pages the variants, and exact reads select precisely with the
    /// identity the list already exposes; body cursors stay bound to that
    /// selection, so even a byte-identical sibling cannot be paged by
    /// another variant's cursor.
    #[tokio::test]
    async fn project_graph_variants_select_and_page_across_sources_and_checkouts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            vec![graph_entry(synthetic_graph(
                project,
                "shared",
                "vary",
                "vary:Node",
                "one",
            ))]
        });
        let workspace_a = "a".repeat(32);
        let workspace_b = "b".repeat(32);
        // The workspace-A overlay repeats the published content hash across
        // a different source and checkout; workspace-B carries distinct
        // bytes so the variant set spans both hazard shapes.
        install_overlay(
            &server,
            &project_id,
            &workspace_a,
            synthetic_graph(&project_id, "shared", "vary", "vary:Node", "one"),
        );
        install_overlay(
            &server,
            &project_id,
            &workspace_b,
            synthetic_graph(&project_id, "shared", "vary", "vary:Node", "two"),
        );
        let expected_schema = serde_json::to_value(
            &synthetic_graph(&project_id, "shared", "vary", "vary:Node", "one").schema,
        )
        .unwrap();

        let listed = server
            .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                project: Some(project_id.clone()),
                provisional: Some("all".into()),
                limit: None,
                offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let listed_text = extract_text(&listed);
        let listed_page: serde_json::Value = serde_json::from_str(&listed_text).unwrap();
        let rows = listed_page["graphs"].as_array().unwrap();
        assert_eq!(rows.len(), 3, "{listed_text}");
        let published_row = rows
            .iter()
            .find(|row| row["source"] == json!("published"))
            .expect("published variant listed");
        let repeated_hash = published_row["content_hash"].as_str().unwrap();
        let overlay_a_row = rows
            .iter()
            .find(|row| row["checkout_id"].as_str() == Some(workspace_a.as_str()))
            .expect("workspace-A overlay listed");
        assert_eq!(
            overlay_a_row["content_hash"],
            json!(repeated_hash),
            "distinct source and checkout repeat one content hash"
        );

        // The multi-variant default summary is a bounded page, not an
        // unbounded graphs array.
        let summary = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let summary_text = summary.clone();
        let summary_page: serde_json::Value = serde_json::from_str(&summary_text).unwrap();
        assert_eq!(summary_page["total"], json!(3), "{summary_text}");
        assert_eq!(summary_page["count"], json!(3), "{summary_text}");
        assert!(summary_page["next_offset"].is_null(), "{summary_text}");
        assert!(
            summary_page["view_stamp"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "{summary_text}"
        );
        let summary_envelope = serde_json::to_vec(&summary_page).unwrap().len();
        assert!(
            summary_envelope < 8192,
            "the complete multi-variant summary stays bounded: {summary_envelope}"
        );

        // Variant paging continues by stamp and refuses a changed set.
        let first = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            Some(1),
            None,
            None,
        )
        .await;
        let first_text = first;
        let first_page: serde_json::Value = serde_json::from_str(&first_text).unwrap();
        assert_eq!(first_page["count"], json!(1), "{first_text}");
        assert_eq!(first_page["next_offset"], json!(1), "{first_text}");
        assert_eq!(
            first_page["graphs"][0]["summary"]["source"],
            json!("provisional")
        );
        let view_stamp = first_page["view_stamp"].as_str().unwrap().to_string();

        let unstamped = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            Some(1),
            Some(1),
            None,
        )
        .await;
        assert!(
            unstamped.contains("error.graph_view_stamp_required"),
            "nonzero variant offsets require the stamp: {unstamped}"
        );

        let second = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            Some(1),
            Some(1),
            Some(view_stamp.clone()),
        )
        .await;
        let second_text = second;
        let second_page: serde_json::Value = serde_json::from_str(&second_text).unwrap();
        assert_eq!(
            second_page["graphs"][0]["summary"]["checkout_id"],
            json!(workspace_b),
            "page two continues into the workspace-B overlay: {second_text}"
        );

        let wrong_stamp = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            Some(1),
            Some(2),
            Some("deadbeef".into()),
        )
        .await;
        assert!(
            wrong_stamp.contains("error.graph_view_changed"),
            "an unknown stamp refuses: {wrong_stamp}"
        );

        install_overlay(
            &server,
            &project_id,
            &"c".repeat(32),
            synthetic_graph(&project_id, "shared", "vary", "vary:Node", "three"),
        );
        let changed = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            Some(1),
            Some(2),
            Some(view_stamp),
        )
        .await;
        assert!(
            changed.contains("error.graph_view_changed"),
            "a changed variant set refuses continuation: {changed}"
        );
        let restarted = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await;
        let restarted_page: serde_json::Value = serde_json::from_str(&restarted).unwrap();
        assert_eq!(restarted_page["total"], json!(4), "{restarted}");

        // A repeated hash alone stays ambiguous; the summary narrows to the
        // two variants that carry it.
        let hash_only = describe_variants_text(
            &server,
            &project_id,
            "shared",
            None,
            None,
            Some(repeated_hash),
            None,
            None,
            None,
        )
        .await;
        let hash_only_page: serde_json::Value = serde_json::from_str(&hash_only).unwrap();
        assert_eq!(hash_only_page["total"], json!(2), "{hash_only}");
        let ambiguous = describe_selected_text(
            &server,
            &project_id,
            "shared",
            "schema",
            None,
            None,
            Some(repeated_hash),
            None,
            None,
        )
        .await;
        assert!(
            ambiguous.contains("error.project_graph_ambiguous")
                && ambiguous.matches("content_hash=").count() >= 2,
            "an ambiguous selector lists every selectable identity: {ambiguous}"
        );

        // Selecting by source (or checkout) resolves the repeated hash, and
        // the exact read reconstructs the selected variant's schema.
        let selected_summary = describe_variants_text(
            &server,
            &project_id,
            "shared",
            Some("published"),
            None,
            Some(repeated_hash),
            None,
            None,
            None,
        )
        .await;
        let selected_page: serde_json::Value = serde_json::from_str(&selected_summary).unwrap();
        assert_eq!(
            selected_page["summary"]["source"],
            json!("published"),
            "the selected variant unwraps alone: {selected_summary}"
        );

        let mut joined = String::new();
        let mut cursor = None;
        loop {
            let text = describe_selected_text(
                &server,
                &project_id,
                "shared",
                "schema",
                Some("published"),
                None,
                Some(repeated_hash),
                cursor,
                None,
            )
            .await;
            let page: serde_json::Value = serde_json::from_str(&text).unwrap();
            joined.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"]
                .as_str()
                .map(ToString::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
            expected_schema
        );

        let mut joined = String::new();
        let mut cursor = None;
        loop {
            let text = describe_selected_text(
                &server,
                &project_id,
                "shared",
                "schema",
                None,
                Some(&workspace_a),
                Some(repeated_hash),
                cursor,
                None,
            )
            .await;
            let page: serde_json::Value = serde_json::from_str(&text).unwrap();
            joined.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"]
                .as_str()
                .map(ToString::to_string);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&joined).unwrap(),
            expected_schema,
            "the workspace-A overlay carries the same bytes; selection resolves it"
        );

        // Body cursors bind to the exact selection: a cursor minted for the
        // published variant refuses under the byte-identical overlay.
        let published_first = describe_selected_text(
            &server,
            &project_id,
            "shared",
            "schema",
            Some("published"),
            None,
            Some(repeated_hash),
            None,
            Some(64),
        )
        .await;
        let published_page: serde_json::Value = serde_json::from_str(&published_first).unwrap();
        let published_cursor = published_page["body"]["next_cursor"]
            .as_str()
            .expect("a 64-byte page always continues")
            .to_string();
        let cross_variant = describe_selected_text(
            &server,
            &project_id,
            "shared",
            "schema",
            None,
            Some(&workspace_a),
            Some(repeated_hash),
            Some(published_cursor),
            None,
        )
        .await;
        assert!(
            cross_variant.contains("restart without cursor"),
            "a cursor must not cross variants that repeat one hash: {cross_variant}"
        );

        let no_match = describe_selected_text(
            &server,
            &project_id,
            "shared",
            "schema",
            None,
            Some(&"f".repeat(32)),
            None,
            None,
            None,
        )
        .await;
        assert!(
            no_match.contains("error.not_found"),
            "a selector that matches nothing is not_found: {no_match}"
        );

        let bad_source = describe_selected_text(
            &server,
            &project_id,
            "shared",
            "schema",
            Some("museum"),
            None,
            None,
            None,
            None,
        )
        .await;
        assert!(
            bad_source.contains("error.bad_input"),
            "unknown source vocabulary refuses: {bad_source}"
        );
    }

    /// A04 follow-up: one huge retrieval-exclusion array must not inflate
    /// the default summary. The count stays inline, the exact sorted list
    /// stays recoverable through the schema body read, and the serialized
    /// summary stays bounded.
    #[tokio::test]
    async fn project_graph_describe_bounds_oversized_retrieval_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            vec![graph_entry(exclusion_heavy_graph(project, 200))]
        });

        let summarized = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project_id.clone(),
                graph_id: "heavy".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let summary_text = extract_text(&summarized);
        let summary_page: serde_json::Value = serde_json::from_str(&summary_text).unwrap();
        assert_eq!(
            summary_page["retrieval"]["excluded_vertex_type_count"],
            json!(200),
            "the count preserves the state without the array: {summary_text}"
        );
        assert!(
            !summary_text.contains("excl:Type1"),
            "the exclusion array must stay out of the summary: {summary_text}"
        );
        let envelope = serde_json::to_vec(&summary_page).unwrap().len();
        assert!(
            envelope < 3072,
            "the serialized summary stays bounded beside 200 exclusions: {envelope}"
        );

        let (joined, pages) =
            collect_describe_body(&server, &project_id, "heavy", "schema", None).await;
        assert!(pages > 1, "the heavy schema spans several body pages");
        let recovered: serde_json::Value = serde_json::from_str(&joined).unwrap();
        let excluded = recovered["index_policy"]["retrieval_excluded_types"]
            .as_array()
            .unwrap();
        assert_eq!(excluded.len(), 200);
        assert_eq!(
            excluded[0],
            json!("excl:Type000"),
            "the exact sorted list recovers from the schema body"
        );
    }

    /// A06: list pages carry totals, byte budgets, and a view-stamp-bound
    /// continuation. A changed inventory refuses instead of paging a
    /// different view.
    #[tokio::test]
    async fn project_graph_list_pages_are_bounded_and_stamp_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            (0..150)
                .map(|idx| {
                    let graph_id = format!("g-{idx:03}");
                    graph_entry(synthetic_graph(
                        project,
                        &graph_id,
                        "inv",
                        "inv:Node",
                        &format!("n{idx:03}"),
                    ))
                })
                .collect::<Vec<_>>()
        });

        let list =
            |limit: Option<usize>, offset: Option<usize>, expected_view_stamp: Option<String>| {
                let server = server.clone();
                let project_id = project_id.clone();
                async move {
                    server
                        .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                            project: Some(project_id),
                            provisional: Some("published".into()),
                            limit,
                            offset,
                            expected_view_stamp,
                        }))
                        .await
                }
            };

        let first = list(None, None, None).await;
        let first_text = extract_text(&first);
        let first_page: serde_json::Value = serde_json::from_str(&first_text).unwrap();
        assert_eq!(first_page["total"], json!(150), "{first_text}");
        assert_eq!(first_page["count"], json!(20), "{first_text}");
        assert_eq!(first_page["next_offset"], json!(20), "{first_text}");
        assert_eq!(
            first_page["graphs"][0]["graph_id"],
            json!("g-000"),
            "{first_text}"
        );
        let view_stamp = first_page["view_stamp"].as_str().unwrap().to_string();
        assert!(!view_stamp.is_empty());
        let first_envelope = serde_json::to_vec(&first_page).unwrap().len();
        assert!(
            first_envelope <= 24_576 + 1_024,
            "the default page plus its continuation metadata stays inside the discovery budget: {first_envelope}"
        );

        let big = list(Some(100), None, None).await;
        let big_text = extract_text(&big);
        let big_page: serde_json::Value = serde_json::from_str(&big_text).unwrap();
        assert_eq!(big_page["total"], json!(150), "{big_text}");
        assert!(
            big_page["count"].as_u64().unwrap() <= 100,
            "summary count must respect the requested cap: {big_text}"
        );
        assert!(
            big_page["next_offset"].as_u64().is_some(),
            "a byte-cut page must continue: {big_text}"
        );
        let big_envelope = serde_json::to_vec(&big_page).unwrap().len();
        assert!(
            big_envelope <= 24_576 + 1_024,
            "byte-cut pages stay inside the discovery budget: {big_envelope}"
        );

        let unstamped = list(None, Some(20), None).await;
        let unstamped_text = extract_text(&unstamped);
        assert!(
            unstamped_text.contains("error.graph_view_stamp_required"),
            "nonzero offsets require the view stamp: {unstamped_text}"
        );

        let second = list(None, Some(20), Some(view_stamp.clone())).await;
        let second_text = extract_text(&second);
        let second_page: serde_json::Value = serde_json::from_str(&second_text).unwrap();
        assert_eq!(
            second_page["graphs"][0]["graph_id"],
            json!("g-020"),
            "continuation resumes at the recorded offset: {second_text}"
        );
        assert_eq!(second_page["next_offset"], json!(40), "{second_text}");
        assert_eq!(second_page["view_stamp"], json!(view_stamp));

        let wrong_stamp = list(None, Some(40), Some("deadbeef".into())).await;
        let wrong_text = extract_text(&wrong_stamp);
        assert!(
            wrong_text.contains("error.graph_view_changed"),
            "an unknown stamp refuses: {wrong_text}"
        );

        // Add one graph: the live view changed, so the old stamp refuses and
        // the caller restarts at offset 0 without a stamp.
        {
            let mut views = server.state.project_graph_views.write();
            let parsed =
                bbox_corpus_core::project_catalog::ProjectId::parse(project_id.clone()).unwrap();
            let mut view = views.published_view(&parsed).unwrap().clone();
            view.graphs.insert(
                "g-150".into(),
                graph_entry(synthetic_graph(
                    project_id.as_str(),
                    "g-150",
                    "inv",
                    "inv:Node",
                    "n150",
                )),
            );
            views.install_published(view);
        }
        let changed = list(None, Some(40), Some(view_stamp)).await;
        let changed_text = extract_text(&changed);
        assert!(
            changed_text.contains("error.graph_view_changed"),
            "a changed inventory refuses continuation: {changed_text}"
        );
        let restarted = list(None, None, None).await;
        let restarted_text = extract_text(&restarted);
        let restarted_page: serde_json::Value = serde_json::from_str(&restarted_text).unwrap();
        assert_eq!(restarted_page["total"], json!(151), "{restarted_text}");
    }

    /// A10/A13: validation summaries page the error rows with a stamp-bound
    /// continuation; detail=errors recovers the complete array byte-exactly;
    /// valid graphs and empty inventories stay distinguishable states.
    #[tokio::test]
    async fn project_graph_validate_pages_errors_and_recovers_the_exact_array() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |project| {
            vec![
                invalid_graph_entry("broken", 55),
                graph_entry(synthetic_graph(project, "tiny", "tiny", "tiny:Node", "n1")),
            ]
        });

        let validate = |graph_id: &str,
                        detail: Option<&str>,
                        error_offset: Option<usize>,
                        expected_error_stamp: Option<String>| {
            let server = server.clone();
            let project_id = project_id.clone();
            let graph_id = graph_id.to_string();
            let detail = detail.map(ToString::to_string);
            async move {
                server
                    .bbox_project_graph_validate(Parameters(ProjectGraphValidateParams {
                        project: project_id,
                        graph_id,
                        provisional: Some("published".into()),
                        source: None,
                        checkout_id: None,
                        expected_content_hash: None,
                        detail,
                        cursor: None,
                        body_limit: None,
                        error_offset,
                        error_limit: None,
                        expected_error_stamp,
                        variant_limit: None,
                        variant_offset: None,
                        expected_view_stamp: None,
                    }))
                    .await
            }
        };

        let first = validate("broken", None, None, None).await;
        let first_text = extract_text(&first);
        let first_page: serde_json::Value = serde_json::from_str(&first_text).unwrap();
        let rows = first_page["graphs"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{first_text}");
        let row = &rows[0];
        assert_eq!(row["valid"], json!(false), "{first_text}");
        assert_eq!(row["source"], json!("published"), "{first_text}");
        assert_eq!(row["errors_total"], json!(55), "{first_text}");
        assert_eq!(row["errors"].as_array().unwrap().len(), 20, "{first_text}");
        assert_eq!(row["errors_offset"], json!(0), "{first_text}");
        assert_eq!(row["next_error_offset"], json!(20), "{first_text}");
        assert_eq!(
            row["errors"][0]["code"],
            json!("edge.missing_vertex"),
            "{first_text}"
        );
        let error_stamp = row["error_stamp"].as_str().unwrap().to_string();
        assert!(!error_stamp.is_empty());
        let first_envelope = serde_json::to_vec(&first_page).unwrap().len();
        assert!(
            first_envelope < 12_288,
            "the default error page stays well inside the response budget: {first_envelope}"
        );

        let second = validate("broken", None, Some(20), Some(error_stamp.clone())).await;
        let second_text = extract_text(&second);
        let second_page: serde_json::Value = serde_json::from_str(&second_text).unwrap();
        let second_row = &second_page["graphs"][0];
        assert_eq!(second_row["errors_offset"], json!(20), "{second_text}");
        assert_eq!(second_row["errors"].as_array().unwrap().len(), 20);
        assert_eq!(second_row["next_error_offset"], json!(40));

        let third = validate("broken", None, Some(40), Some(error_stamp.clone())).await;
        let third_text = extract_text(&third);
        let third_page: serde_json::Value = serde_json::from_str(&third_text).unwrap();
        let third_row = &third_page["graphs"][0];
        assert_eq!(third_row["errors"].as_array().unwrap().len(), 15);
        assert!(third_row["next_error_offset"].is_null(), "{third_text}");

        let unstamped = validate("broken", None, Some(20), None).await;
        let unstamped_text = extract_text(&unstamped);
        assert!(
            unstamped_text.contains("error.graph_error_stamp_required"),
            "nonzero error offsets require the error stamp: {unstamped_text}"
        );

        let wrong = validate("broken", None, Some(20), Some("deadbeef".into())).await;
        let wrong_text = extract_text(&wrong);
        assert!(
            wrong_text.contains("error.graph_errors_changed"),
            "an unknown error stamp refuses: {wrong_text}"
        );

        // Exact recovery: the paged array reconstructs byte-for-byte and
        // parses, in both default and tiny page sizes.
        let expected_errors: Vec<serde_json::Value> = synthetic_validation_errors(55)
            .iter()
            .map(|error| serde_json::to_value(error).unwrap())
            .collect();
        let joined = collect_validation_errors(&server, &project_id, "broken", None).await;
        let recovered: Vec<serde_json::Value> = serde_json::from_str(&joined).unwrap();
        assert_eq!(recovered, expected_errors);
        assert!(
            joined.contains("缺口"),
            "multibyte messages survive page boundaries: {joined}"
        );
        let tiny_joined = collect_validation_errors(&server, &project_id, "broken", Some(64)).await;
        let tiny_recovered: Vec<serde_json::Value> = serde_json::from_str(&tiny_joined).unwrap();
        assert_eq!(tiny_recovered, expected_errors);

        // Empty states stay explicit rather than collapsing to nulls.
        let valid = validate("tiny", None, None, None).await;
        let valid_text = extract_text(&valid);
        let valid_page: serde_json::Value = serde_json::from_str(&valid_text).unwrap();
        assert_eq!(valid_page["graphs"][0]["valid"], json!(true));
        assert_eq!(valid_page["graphs"][0]["errors"], json!([]));
        assert_eq!(valid_page["graphs"][0]["errors_total"], json!(0));
        let empty_errors = collect_validation_errors(&server, &project_id, "tiny", None).await;
        assert_eq!(empty_errors, "[]");

        // The invalid entry keeps its summary identity but has no exact
        // schema payload to page.
        let unavailable =
            describe_detail_text(&server, &project_id, "broken", "schema", None, None).await;
        assert!(
            unavailable.contains("error.graph_payload_unavailable"),
            "invalid entries distinguish unavailable payloads: {unavailable}"
        );
        assert!(
            unavailable.contains("bbox_project_graph_validate"),
            "the refusal names the diagnostics tool: {unavailable}"
        );

        // A changed error set refuses the old stamp.
        {
            let mut views = server.state.project_graph_views.write();
            let parsed =
                bbox_corpus_core::project_catalog::ProjectId::parse(project_id.clone()).unwrap();
            let mut view = views.published_view(&parsed).unwrap().clone();
            view.graphs
                .insert("broken".into(), invalid_graph_entry("broken", 1));
            views.install_published(view);
        }
        let changed = validate("broken", None, Some(0), Some(error_stamp)).await;
        let changed_text = extract_text(&changed);
        assert!(
            changed_text.contains("error.graph_errors_changed"),
            "a changed error set refuses continuation: {changed_text}"
        );
    }

    /// A06: an inventory with no graphs and a missing graph stay distinct,
    /// explicit empty and not_found states instead of empty pages.
    #[tokio::test]
    async fn project_graph_list_reports_empty_inventory_and_missing_graphs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_published_entries(&server, &root, |_| Vec::new());

        let listed = server
            .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                project: Some(project_id.clone()),
                provisional: Some("published".into()),
                limit: None,
                offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let listed_text = extract_text(&listed);
        let listed_page: serde_json::Value = serde_json::from_str(&listed_text).unwrap();
        assert_eq!(listed_page["total"], json!(0), "{listed_text}");
        assert_eq!(listed_page["graphs"], json!([]), "{listed_text}");
        assert!(listed_page["next_offset"].is_null(), "{listed_text}");
        assert!(
            listed_page["view_stamp"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "even an empty view stamps its continuation state"
        );

        let missing =
            describe_detail_text(&server, &project_id, "nowhere", "schema", None, None).await;
        assert!(
            missing.contains("error.not_found"),
            "a missing graph is a not_found state, not an empty page: {missing}"
        );
    }

    /// Evidence is an edge family on BOTH endpoints of a binding, so the same
    /// assertion is discoverable from either vertex.
    #[tokio::test]
    async fn inspect_surfaces_evidence_edges_on_both_endpoints() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let (project_id, _) = install_evidence_fixture(&server, &root);

        let record = inspect_published(
            &server,
            &format!("project_graph_vertex:{project_id}:records:filing-1"),
        )
        .await;
        let out = record["edges"]["out"].as_array().unwrap();
        let binding = out
            .iter()
            .find(|edge| edge["kind"] == "record:CORRESPONDS_TO")
            .unwrap_or_else(|| panic!("record vertex carries its evidence edge: {record}"));
        assert_eq!(binding["properties"]["evidence.freshness"], "current");
        assert_eq!(
            binding["properties"]["evidence.assertion_authority"],
            "project"
        );

        let source = inspect_published(
            &server,
            &format!("project_graph_vertex:{project_id}:source:asset-1"),
        )
        .await;
        let incoming = source["edges"]["in"].as_array().unwrap();
        assert!(
            incoming
                .iter()
                .any(|edge| edge["kind"] == "record:CORRESPONDS_TO"),
            "the same binding must appear on the target endpoint: {source}"
        );
        let outgoing = source["edges"]["out"].as_array().unwrap();
        assert!(
            outgoing
                .iter()
                .any(|edge| edge["kind"] == "dataset:EVIDENCED_BY"),
            "the source vertex must carry its outgoing file binding: {source}"
        );
    }

    /// Contract: connector reprojection changes freshness status but cannot
    /// delete a tenant-owned binding. Advancing the source graph to a
    /// generation the binding never saw leaves the edge in place and marked
    /// stale rather than dropping it.
    #[tokio::test]
    async fn reprojection_marks_a_binding_stale_without_deleting_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let (project_id, _) = install_evidence_fixture(&server, &root);

        // Reproject the source graph: same vertex, later generation.
        {
            let mut views = server.state.project_graph_views.write();
            let parsed =
                bbox_corpus_core::project_catalog::ProjectId::parse(project_id.clone()).unwrap();
            let mut view = views.published_view(&parsed).unwrap().clone();
            let mut reprojected =
                synthetic_graph(&project_id, "source", "dataset", "dataset:Asset", "asset-1");
            reprojected.descriptor.generation = 2;
            view.graphs
                .insert("source".to_string(), graph_entry(reprojected));
            views.install_published(view);
        }

        let inspected = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                edge_cursor: None,
                property: None,
                property_cursor: None,
                property_limit: None,
                entity_ref: format!("project_graph_vertex:{project_id}:records:filing-1"),
                provisional: Some("published".into()),
                edge_types: None,
                direction: Some("both".into()),
                per_type_limit: Some(10),
                property_mode: Some("full".into()),
            }))
            .await;
        let text = extract_text(&inspected);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let binding = value["edges"]["out"]
            .as_array()
            .unwrap()
            .iter()
            .find(|edge| edge["kind"] == "record:CORRESPONDS_TO")
            .unwrap_or_else(|| panic!("the binding survives reprojection: {text}"));
        assert_eq!(
            binding["properties"]["evidence.target_status"], "stale",
            "reprojection must move freshness: {text}"
        );
        assert_eq!(binding["properties"]["evidence.freshness"], "stale");
    }

    /// Contract: bindings and their provenance appear in bundles.
    #[tokio::test]
    async fn bundles_carry_evidence_bindings_and_their_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let (project_id, _) = install_evidence_fixture(&server, &root);

        let bundled = server
            .bbox_bundle_evidence(Parameters(BundleEvidenceParams {
                question: "what does this filing correspond to?".into(),
                entity_refs: vec![
                    format!("project_graph_vertex:{project_id}:records:filing-1"),
                    format!("project_graph_vertex:{project_id}:source:asset-1"),
                ],
                path_ids: Vec::new(),
                provisional: Some("published".into()),
                property_mode: Some("summary".into()),
                cursor: None,
                body_limit: None,
            }))
            .await;
        let text = extract_text(&bundled);
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        let edges = value["intra_bundle_edges"]
            .as_array()
            .unwrap_or_else(|| panic!("bundle must carry the binding: {text}"));
        let binding = edges
            .iter()
            .find(|edge| edge["kind"] == "record:CORRESPONDS_TO")
            .unwrap_or_else(|| panic!("{text}"));
        assert_eq!(
            binding["properties"]["evidence.binding_id"],
            "record-to-source"
        );
        assert_eq!(
            binding["properties"]["evidence.assertion_authority"],
            "project"
        );
        assert_eq!(
            binding["properties"]["evidence.mapping_version"],
            "mapping-v1"
        );
    }

    #[tokio::test]
    async fn project_graph_tools_inspect_and_traverse_governance_fixture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_governance_graph(&server, &root);

        let listed = server
            .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                project: Some(project_id.clone()),
                provisional: Some("published".into()),
                limit: None,
                offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let listed_text = extract_text(&listed);
        assert!(listed_text.contains("governance-record"), "{listed_text}");
        let listed_page: serde_json::Value = serde_json::from_str(&listed_text).unwrap();
        assert_eq!(listed_page["total"], 1);
        assert_eq!(listed_page["graphs"].as_array().unwrap().len(), 1);
        assert!(
            listed_page["view_stamp"]
                .as_str()
                .is_some_and(|s| !s.is_empty())
        );

        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let described_text = extract_text(&described);
        assert!(
            described_text.contains("governance-record-schema"),
            "{described_text}"
        );
        assert!(
            !described_text.contains("gov:Record"),
            "the compact summary must not embed the schema body: {described_text}"
        );

        let validated = server
            .bbox_project_graph_validate(Parameters(ProjectGraphValidateParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                error_offset: None,
                error_limit: None,
                expected_error_stamp: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&extract_text(&validated)).unwrap()["graphs"]
                [0]["valid"],
            true
        );

        let vertex_ref =
            format!("project_graph_vertex:{project_id}:governance-record:record/case@1");
        let inspected = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                edge_cursor: None,
                property: None,
                property_cursor: None,
                property_limit: None,
                entity_ref: vertex_ref.clone(),
                provisional: Some("published".into()),
                edge_types: None,
                direction: Some("both".into()),
                per_type_limit: Some(10),
                property_mode: Some("full".into()),
            }))
            .await;
        let inspected_text = extract_text(&inspected);
        assert!(inspected_text.contains("record/case@1"), "{inspected_text}");

        let paths = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: vertex_ref,
                provisional: Some("published".into()),
                to: None,
                to_type: Some("project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(2),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        assert!(extract_text(&paths).contains("\"paths\""));
    }

    /// gap-e41499a9: under own visibility an authored graph materializes as
    /// `provisional_project_graph_vertex` overlay refs, so a caller filtering
    /// on the logical type used to walk the whole neighborhood and match
    /// nothing. The logical type must be enough; the overlay type name stays
    /// available for callers that want the overlay form exclusively.
    #[tokio::test]
    async fn find_paths_to_type_admits_overlay_vertices_under_own_visibility() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_governance_graph(&server, &root);
        let scope = PublishedScope::try_new("repo-governance", ".").unwrap();
        let workspace_id = bro_core::WorkspaceId::parse("a".repeat(32)).unwrap();
        server.set_session_checkout_for_test(
            project_id.clone(),
            scope.clone(),
            workspace_id.to_string(),
            root.clone(),
        );
        let graph = load_governance_generation(&project_id, &root);
        let content_hash = graph.fingerprint.clone();
        server
            .state
            .project_graph_views
            .write()
            .install_provisional(
                bbox_indexing::project_graph_view::ProvisionalProjectGraphOverlay {
                    project_id: bbox_corpus_core::project_catalog::ProjectId::parse(
                        project_id.clone(),
                    )
                    .unwrap(),
                    scope,
                    workspace_id: workspace_id.clone(),
                    source_generation_id: "working-one".into(),
                    graphs: std::collections::BTreeMap::from([(
                        "governance-record".into(),
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(
                            bbox_indexing::project_graph_view::ProjectGraphViewEntry::valid(
                                "governance-record".into(),
                                bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                                    accepted_generation: "generation-one".into(),
                                    accepted_commit: "a".repeat(40),
                                    source_generation: Some("working-one".into()),
                                    workspace_id: Some(workspace_id),
                                    content_hash,
                                },
                                graph,
                            ),
                        ),
                    )]),
                    evidence: None,
                },
            );

        let vertex_ref =
            format!("project_graph_vertex:{project_id}:governance-record:record/case@1");
        let logical = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: vertex_ref.clone(),
                provisional: Some("own".into()),
                to: None,
                to_type: Some("project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(2),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let logical_text = extract_text(&logical);
        assert!(!logical_text.contains("No paths found"), "{logical_text}");
        assert!(
            logical_text.contains("provisional_project_graph_vertex:"),
            "{logical_text}"
        );

        // The overlay type name keeps working for callers that already know it.
        let explicit = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: vertex_ref,
                provisional: Some("own".into()),
                to: None,
                to_type: Some("provisional_project_graph_vertex".into()),
                edge_types: None,
                max_depth: Some(2),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let explicit_text = extract_text(&explicit);
        assert!(!explicit_text.contains("No paths found"), "{explicit_text}");
        assert!(
            explicit_text.contains("provisional_project_graph_vertex:"),
            "{explicit_text}"
        );
    }

    /// gap-e41499a9: a targetless call is a malformed call, not an empty
    /// neighborhood, and must say so at the tool boundary.
    #[tokio::test]
    async fn find_paths_without_a_target_refuses_at_the_tool_boundary() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_governance_graph(&server, &root);

        let refused = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: format!("project_graph_vertex:{project_id}:governance-record:record/case@1"),
                provisional: Some("published".into()),
                to: None,
                to_type: None,
                edge_types: None,
                max_depth: Some(2),
                limit: Some(5),
                max_fanout: None,
            }))
            .await;
        let refused_text = extract_text(&refused);
        assert!(refused_text.contains("error.bad_input"), "{refused_text}");
        assert!(refused_text.contains("suggested_fix"), "{refused_text}");
        assert!(!refused_text.contains("No paths found"), "{refused_text}");
    }

    #[tokio::test]
    async fn project_graph_tools_surface_invalid_own_overlay_without_fallback() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project_id = install_governance_graph(&server, &root);
        let scope = PublishedScope::try_new("repo-governance", ".").unwrap();
        let workspace_id = bro_core::WorkspaceId::parse("a".repeat(32)).unwrap();
        server.set_session_checkout_for_test(
            project_id.clone(),
            scope.clone(),
            workspace_id.to_string(),
            root,
        );
        server
            .state
            .project_graph_views
            .write()
            .install_provisional(
                bbox_indexing::project_graph_view::ProvisionalProjectGraphOverlay {
                    project_id: bbox_corpus_core::project_catalog::ProjectId::parse(
                        project_id.clone(),
                    )
                    .unwrap(),
                    scope,
                    workspace_id: workspace_id.clone(),
                    source_generation_id: "working-one".into(),
                    graphs: std::collections::BTreeMap::from([(
                        "governance-record".into(),
                        bbox_indexing::project_graph_view::ProjectGraphOverlayValue::Upsert(
                            bbox_indexing::project_graph_view::ProjectGraphViewEntry::invalid(
                                "governance-record".into(),
                                bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                                    accepted_generation: "generation-one".into(),
                                    accepted_commit: "a".repeat(40),
                                    source_generation: Some("working-one".into()),
                                    workspace_id: Some(workspace_id),
                                    content_hash: "invalid-content".into(),
                                },
                                vec![bbox_project_graph::ValidationError::new(
                                    "edge.missing_vertex",
                                    "edges.jsonl",
                                    Some(7),
                                    "edge target is missing",
                                )],
                            ),
                        ),
                    )]),
                    evidence: None,
                },
            );

        let own = server
            .bbox_project_graph_validate(Parameters(ProjectGraphValidateParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                provisional: Some("own".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                error_offset: None,
                error_limit: None,
                expected_error_stamp: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let own_text = extract_text(&own);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&own_text).unwrap()["graphs"][0]["valid"],
            false
        );
        assert!(own_text.contains("edge.missing_vertex"), "{own_text}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&own_text).unwrap()["graphs"][0]["source"],
            "provisional"
        );

        let published = server
            .bbox_project_graph_validate(Parameters(ProjectGraphValidateParams {
                project: project_id,
                graph_id: "governance-record".into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                error_offset: None,
                error_limit: None,
                expected_error_stamp: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&extract_text(&published)).unwrap()["graphs"]
                [0]["valid"],
            true
        );
    }

    #[tokio::test]
    async fn workspace_blame_plan_and_fact_join_never_acquire_a_checkout() {
        use bbox_corpus_core::blame_transport::{
            BLAME_TRANSPORT_VERSION, BlameExecutionV1, BlameFactV1,
        };

        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        assert!(
            server
                .session_workspace_binding
                .set(Some(Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "task".into(),
                        session_id: "session".into(),
                        project_id: "project-bound".into(),
                        scope: PublishedScope::try_new("repo-bound", ".").unwrap(),
                        workspace_id: bro_core::WorkspaceId::parse("a".repeat(32)).unwrap(),
                        expires_unix_secs: u64::MAX,
                    },
                )))
                .is_ok()
        );
        let before = server.state.checkout_access.health().sequence;

        let planned = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Plan),
            }))
            .await;
        assert_ne!(planned.is_error, Some(true), "{}", extract_text(&planned));
        let planned: serde_json::Value = serde_json::from_str(&extract_text(&planned)).unwrap();
        let plan: bbox_corpus_core::blame_transport::BlameExecutionPlanV1 =
            serde_json::from_value(planned["plan"].clone()).unwrap();
        assert_eq!(plan.project_id, "project-bound");
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let foreign = mcp_tools::blame::BlameTargetIdentity::ProjectFile {
            project_id: "project-foreign".into(),
            indexed_path_hint: PathBuf::from("src/lib.rs"),
            line: Some(7),
            byte_offset: 0,
        };
        let foreign_overlays = std::collections::BTreeMap::from([(
            "project-foreign".to_string(),
            bbox_corpus_core::git_overlay::GitOverlaySelector {
                project_id: "project-foreign".into(),
                code_generation: "cg_test".into(),
                repo_history_generation: "rhg_test".into(),
                source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
                    attachment_id: "attachment-foreign".into(),
                },
                repo_head: "b".repeat(40),
                commit_namespace: "ns".into(),
                overlay_generation: 1,
            },
        )]);
        let foreign_error = workspace_blame_plan(&server, &foreign, &foreign_overlays).unwrap_err();
        assert!(format!("{foreign_error:#}").contains("different project"));

        let fact = BlameFactV1 {
            version: BLAME_TRANSPORT_VERSION,
            project_id: plan.project_id.clone(),
            scope: plan.scope.clone(),
            workspace_id: plan.workspace_id.clone(),
            git_relative_path: "src/lib.rs".into(),
            display_path: "src/lib.rs".into(),
            line: 7,
            execution: BlameExecutionV1::WorkspaceCurrent {
                head_commit: Some("b".repeat(40)),
            },
            attribution: None,
        };
        let mut stale_plan = plan.clone();
        if let bbox_corpus_core::blame_transport::BlamePlanTargetV1::WorkspacePath {
            line, ..
        } = &mut stale_plan.target
        {
            *line = 8;
        }
        let stale = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Resolve {
                    plan: stale_plan,
                    fact: fact.clone(),
                }),
            }))
            .await;
        assert_eq!(stale.is_error, Some(true));
        assert!(extract_text(&stale).contains("error.blame_plan_stale"));
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let resolved = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Resolve {
                    plan: plan.clone(),
                    fact: fact.clone(),
                }),
            }))
            .await;
        assert_eq!(resolved.is_error, Some(true), "{}", extract_text(&resolved));
        let resolved_text = extract_text(&resolved);
        assert!(resolved_text.contains("error.not_found"));
        let resolved_value: serde_json::Value = serde_json::from_str(&resolved_text).unwrap();
        let resolved_sha256 =
            hex::encode(Sha256::digest(serde_json::to_vec(&resolved_value).unwrap()));

        let compared = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Compare {
                    plan: plan.clone(),
                    fact: fact.clone(),
                    legacy_response_sha256: resolved_sha256,
                }),
            }))
            .await;
        assert_eq!(compared.is_error, Some(true), "{}", extract_text(&compared));
        assert_eq!(extract_text(&compared), resolved_text);

        let mismatch = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Compare {
                    plan,
                    fact,
                    legacy_response_sha256: "d".repeat(64),
                }),
            }))
            .await;
        assert_eq!(mismatch.is_error, Some(true));
        assert!(extract_text(&mismatch).contains("error.blame_locality_mismatch"));
        let observations = server.state.blame_locality_observations.snapshot();
        assert_eq!(observations.sequence, 3);
        assert_eq!(observations.comparisons.len(), 1);
        assert!(!observations.comparisons[0].equal);
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let fallback = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_eq!(fallback.is_error, Some(true));
        assert!(extract_text(&fallback).contains("error.blame_locality_required"));
        assert_eq!(server.state.checkout_access.health().sequence, before);

        let expired_tmp = tempfile::tempdir().unwrap();
        let expired_server = test_server(&expired_tmp);
        assert!(
            expired_server
                .session_workspace_binding
                .set(Some(Arc::new(
                    crate::server::knowledge_source::WorkspaceBindingGrant {
                        task_id: "expired-task".into(),
                        session_id: "expired-session".into(),
                        project_id: "project-bound".into(),
                        scope: PublishedScope::try_new("repo-bound", ".").unwrap(),
                        workspace_id: bro_core::WorkspaceId::parse("a".repeat(32)).unwrap(),
                        expires_unix_secs: 0,
                    },
                )))
                .is_ok()
        );
        let expired_before = expired_server.state.checkout_access.health().sequence;
        let expired = expired_server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Plan),
            }))
            .await;
        assert_eq!(expired.is_error, Some(true));
        assert!(extract_text(&expired).contains("workspace binding has expired"));
        assert_eq!(
            expired_server.state.checkout_access.health().sequence,
            expired_before
        );
    }

    #[tokio::test]
    async fn operator_blame_authority_is_locality_only_and_never_acquires_a_checkout() {
        use bbox_corpus_core::blame_transport::{
            BLAME_TRANSPORT_VERSION, BlameExecutionV1, BlameFactV1,
        };

        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        assert!(
            server
                .session_operator_blame_binding
                .set(Some(Arc::new(
                    crate::server::blame_authority::OperatorBlameGrant {
                        project_id: "project-bound".into(),
                        scope: PublishedScope::try_new("repo-bound", ".").unwrap(),
                        workspace_id: bro_core::WorkspaceId::parse("a".repeat(32)).unwrap(),
                    },
                )))
                .is_ok()
        );
        let before = server.state.checkout_access.health().sequence;
        let planned = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Plan),
            }))
            .await;
        assert_ne!(planned.is_error, Some(true), "{}", extract_text(&planned));
        let planned: serde_json::Value = serde_json::from_str(&extract_text(&planned)).unwrap();
        let plan: bbox_corpus_core::blame_transport::BlameExecutionPlanV1 =
            serde_json::from_value(planned["plan"].clone()).unwrap();
        let fact = BlameFactV1 {
            version: BLAME_TRANSPORT_VERSION,
            project_id: plan.project_id.clone(),
            scope: plan.scope.clone(),
            workspace_id: plan.workspace_id.clone(),
            git_relative_path: "src/lib.rs".into(),
            display_path: "src/lib.rs".into(),
            line: 7,
            execution: BlameExecutionV1::WorkspaceCurrent { head_commit: None },
            attribution: None,
        };
        let resolved = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: Some(mcp_tools::blame::BlameLocalityRequestV1::Resolve { plan, fact }),
            }))
            .await;
        assert_eq!(resolved.is_error, Some(true), "{}", extract_text(&resolved));
        assert!(extract_text(&resolved).contains("error.not_found"));

        let fallback = server
            .bbox_blame(Parameters(BlameParams {
                file: Some("src/lib.rs".into()),
                line: Some(7),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_eq!(fallback.is_error, Some(true));
        assert!(extract_text(&fallback).contains("error.blame_locality_required"));
        assert_eq!(server.state.checkout_access.health().sequence, before);
    }

    #[derive(Clone)]
    struct RecordingAuthority {
        roots: Arc<HashMap<String, PathBuf>>,
        published_scopes: Arc<HashMap<String, PublishedScope>>,
        requests: Arc<Mutex<Vec<CheckoutAccessRequest>>>,
        resolves: Arc<AtomicUsize>,
    }

    impl CheckoutAccessAuthority for RecordingAuthority {
        fn resolve(
            &self,
            request: &CheckoutAccessRequest,
        ) -> std::result::Result<CheckoutAccessCandidate, CheckoutAccessError> {
            self.resolves.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request.clone());
            let root = self
                .roots
                .get(&request.project_id)
                .cloned()
                .ok_or_else(|| {
                    CheckoutAccessError::new(
                        bbox_indexing::checkout_access::CheckoutAccessErrorCode::AttachmentNotFound,
                        "test project has no attachment",
                    )
                })?;
            let checkout_id = match &request.attachment {
                CheckoutAttachmentSelector::CheckoutId(checkout_id) => checkout_id.clone(),
                _ => format!("selected-{}", request.project_id),
            };
            Ok(CheckoutAccessCandidate {
                project_id: request.project_id.clone(),
                attachment_id: format!("attachment-{}", request.project_id),
                checkout_id,
                branch_ref: None,
                published_scope: self
                    .published_scopes
                    .get(&request.project_id)
                    .cloned()
                    .or_else(|| request.expected_scope.clone()),
                checkout_root: root.clone(),
                project_root: root,
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

    fn test_record(root: &Path, project_id: &str) -> ProjectRecord {
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

    fn recording_broker(
        roots: HashMap<String, PathBuf>,
    ) -> (
        CheckoutAccessBroker,
        Arc<Mutex<Vec<CheckoutAccessRequest>>>,
        Arc<AtomicUsize>,
    ) {
        recording_broker_with_scopes(roots, HashMap::new())
    }

    fn recording_broker_with_scopes(
        roots: HashMap<String, PathBuf>,
        published_scopes: HashMap<String, PublishedScope>,
    ) -> (
        CheckoutAccessBroker,
        Arc<Mutex<Vec<CheckoutAccessRequest>>>,
        Arc<AtomicUsize>,
    ) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let resolves = Arc::new(AtomicUsize::new(0));
        let authority = RecordingAuthority {
            roots: Arc::new(roots),
            published_scopes: Arc::new(published_scopes),
            requests: requests.clone(),
            resolves: resolves.clone(),
        };
        (
            CheckoutAccessBroker::new(Arc::new(authority), CheckoutAccessObservations::in_memory()),
            requests,
            resolves,
        )
    }

    #[test]
    fn selected_operation_discovers_then_pins_exact_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let scope = PublishedScope::try_new("repo-one", ".").unwrap();
        let (broker, requests, _) = recording_broker_with_scopes(
            HashMap::from([("project-one".into(), root)]),
            HashMap::from([("project-one".into(), scope.clone())]),
        );

        let server = test_server(&dir);
        let lease = acquire_selected_operation(
            &server,
            &broker,
            "project-one",
            CheckoutAccessKind::Blame,
            CheckoutAccessIntent::Read,
        )
        .unwrap();

        assert_eq!(lease.published_scope(), Some(&scope));
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].kind,
            CheckoutAccessKind::PublisherConfigTreeRead
        );
        assert_eq!(requests[0].expected_scope, None);
        assert_eq!(requests[1].kind, CheckoutAccessKind::Blame);
        assert_eq!(requests[1].expected_scope, Some(scope));
    }

    #[test]
    fn deny_checkout_access_returns_before_any_file_or_git_input_exists() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("file.rs"), "not a repository").unwrap();
        let broker = CheckoutAccessBroker::new(
            Arc::new(DenyCheckoutAccess),
            CheckoutAccessObservations::in_memory(),
        );
        let project = test_record(&root, "project-one");

        let server = test_server(&dir);
        let error = acquire_file_selection(
            &server,
            &broker,
            selected_file_selection(project.project_id, PathBuf::from("file.rs")),
            CheckoutAccessKind::Blame,
            CheckoutAccessIntent::Read,
        )
        .unwrap_err();

        assert!(error.to_string().contains("denied_by_test_probe"));
        let operation = broker
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::PublisherConfigTreeRead)
            .unwrap();
        assert_eq!(operation.granted, 0);
        assert_eq!(operation.denied, 1);
    }

    #[test]
    fn session_relative_file_uses_exact_checkout_id_selector() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(root.join("file.rs"), "fn main() {}\n").unwrap();
        let project = test_record(&root, "project-one");
        let (broker, requests, _) =
            recording_broker(HashMap::from([(project.project_id.clone(), root.clone())]));
        let session = ResolvedCheckoutScope {
            project_id: project.project_id.clone(),
            published_scope: PublishedScope::try_new("repo-one", ".").unwrap(),
            checkout_id: "checkout-one".into(),
            checkout_dir: root.to_string_lossy().into_owned(),
            checkout_project_dir: root.to_string_lossy().into_owned(),
            branch_ref: None,
        };
        let selection = relative_file_selection(
            &broker,
            Path::new("file.rs"),
            None,
            Some(&session),
            std::slice::from_ref(&project),
            &[],
        )
        .unwrap();

        let server = test_server(&dir);
        let acquired = acquire_file_selection(
            &server,
            &broker,
            selection,
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap();

        assert_eq!(acquired.relative_path, "file.rs");
        let requests = requests.lock().unwrap();
        assert!(requests.iter().all(|request| {
            request.attachment == CheckoutAttachmentSelector::CheckoutId("checkout-one".into())
                && request.source_lane == CheckoutAccessSourceLane::LegacyCheckoutRegistry
        }));
    }

    #[test]
    fn absolute_checkout_path_matches_row_by_discovered_full_scope() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let base = root.join("base");
        let checkout = root.join("checkout");
        std::fs::create_dir(&base).unwrap();
        std::fs::create_dir(&checkout).unwrap();
        std::fs::write(checkout.join("file.rs"), "fn main() {}\n").unwrap();
        let project = test_record(&base, "project-one");
        assert_eq!(
            project.repo_id.as_deref(),
            None,
            "weak repo hint must not be required"
        );
        let scope = PublishedScope::try_new("repo-one", ".").unwrap();
        let row = CheckoutRow {
            project_id: None,
            checkout_id: "checkout-one".into(),
            checkout_dir: checkout.to_string_lossy().into_owned(),
            repo_id: Some(scope.repo_id().to_string()),
            bbox_root_relpath: Some(scope.bbox_root_relpath().to_string()),
            branch_ref: None,
        };
        let (broker, requests, _) = recording_broker_with_scopes(
            HashMap::from([(project.project_id.clone(), checkout.clone())]),
            HashMap::from([(project.project_id.clone(), scope)]),
        );

        let selection = absolute_file_selection(
            &broker,
            &checkout.join("file.rs"),
            std::slice::from_ref(&project),
            std::slice::from_ref(&row),
        )
        .unwrap();
        assert_eq!(
            selection.attachment,
            CheckoutAttachmentSelector::CheckoutId("checkout-one".into())
        );
        let server = test_server(&dir);
        let acquired = acquire_file_selection(
            &server,
            &broker,
            selection,
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap();
        assert_eq!(acquired.relative_path, "file.rs");
        assert!(requests.lock().unwrap().iter().any(|request| {
            request.attachment == CheckoutAttachmentSelector::CheckoutId("checkout-one".into())
        }));
    }

    #[test]
    fn relative_path_escape_is_rejected_after_lease_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let project = test_record(&root, "project-one");
        let (broker, _, _) = recording_broker(HashMap::from([(project.project_id.clone(), root)]));

        let server = test_server(&dir);
        let error = acquire_file_selection(
            &server,
            &broker,
            selected_file_selection(project.project_id, PathBuf::from("../escape.rs")),
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap_err();

        assert!(error.to_string().contains("unsafe_relative_path"));
    }

    #[test]
    fn provenance_all_projects_acquires_one_lease_per_project() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let first = root.join("first");
        let second = root.join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let projects = vec![
            test_record(&first, "project-one"),
            test_record(&second, "project-two"),
        ];
        let (broker, requests, resolves) = recording_broker(HashMap::from([
            ("project-one".into(), first),
            ("project-two".into(), second),
        ]));

        let server = test_server(&dir);
        let (leases, inputs) = acquire_provenance_projects(
            &server,
            &broker,
            &ProvenanceParams { project_id: None },
            &projects,
            CheckoutAccessIntent::Read,
        )
        .unwrap();

        assert_eq!(leases.len(), 2);
        assert_eq!(inputs.len(), 2);
        assert_eq!(resolves.load(Ordering::SeqCst), 4);
        let requests = requests.lock().unwrap();
        assert_eq!(requests.len(), 4);
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.kind == CheckoutAccessKind::PublisherConfigTreeRead)
                .count(),
            2
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.kind == CheckoutAccessKind::ProvenanceNoteIo)
                .count(),
            2
        );
        assert!(
            requests
                .iter()
                .all(|request| request.attachment == CheckoutAttachmentSelector::Selected)
        );
    }
    /// Symbols are edge-projected vertices: the indexer derives their edges
    /// but writes no entity doc (gap-496fe07f). A symbol ref the edge
    /// sidecar names must inspect OK (existence = edge participation); a
    /// well-formed ref nothing points at must stay not_found.
    #[tokio::test]
    async fn inspect_resolves_edge_projected_symbol_without_entity_doc() {
        use bbox_chunker::{EdgeConfidence, EdgeProvenance};
        use bbox_edge_index::edge_index::{Edge, EdgeIndex};

        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let symbol = "symbol:d723917f:KnowledgeStore:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let file = "project_file:d723917f:31d088f0:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb:163";
        let selectors = server.state.idx.read().active_code_selectors();
        let searcher = server.state.idx.read().searcher();
        *server.state.code_read_view.write() = std::sync::Arc::new(crate::server::CodeReadView {
            active_selectors: selectors,
            searcher,
            edge_index: std::sync::Arc::new(EdgeIndex::from_edges_for_tests(vec![Edge {
                source: crate::entity_ref::EntityRef::parse(symbol).unwrap(),
                kind: "DEFINED_IN".into(),
                target: crate::entity_ref::EntityRef::parse(file).unwrap(),
                provenance: EdgeProvenance::Derived,
                confidence: EdgeConfidence::Exact,
                metadata: Default::default(),
                project_id: None,
            }])),
            catalog_epoch: 0,
            git_overlays: std::collections::BTreeMap::new(),
        });

        let inspect = |entity_ref: String| {
            let server = server.clone();
            async move {
                let result = server
                    .bbox_inspect_entity(Parameters(InspectEntityParams {
                        edge_cursor: None,
                        property: None,
                        property_cursor: None,
                        property_limit: None,
                        entity_ref,
                        provisional: None,
                        edge_types: None,
                        direction: None,
                        per_type_limit: Some(5),
                        property_mode: Some("full".into()),
                    }))
                    .await;
                serde_json::from_str::<serde_json::Value>(&extract_text(&result)).unwrap()
            }
        };

        let found = inspect(symbol.to_string()).await;
        assert_eq!(
            found["status"], "ok",
            "edge-backed symbol must inspect: {found}"
        );
        assert_eq!(found["properties"]["qualified_name"], "KnowledgeStore");
        assert_eq!(found["properties"]["source"], "edge_projection");

        let orphan =
            inspect("symbol:d723917f:KnowledgeStore:cccccccccccccccccccccccccccccccc".to_string())
                .await;
        assert_eq!(
            orphan["status"], "error.not_found",
            "edge-less symbol ref must stay not_found: {orphan}"
        );
    }

    #[test]
    fn bbox_describe_schema_omits_installed_agents_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "schema-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "schema-tester",
                "version": 1,
                "manifest": {
                    "description": "Agent for schema test.",
                    "when_to_use": ["use when testing schema"],
                    "anti_patterns": ["do not use in prod"],
                    "brofile_inline": {"provider": "claude"},
                    "cost_class": "normal",
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "badgey-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "badgey-agent",
                "version": 3,
                "manifest": {
                    "description": "Badgey-backed agent.",
                    "brofile_inline": {"provider": "claude"},
                    "cost_class": "cheap",
                    "dispatch_adapter": "badgey",
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        drop(cat);

        let result = server.bbox_describe_schema(Parameters(DescribeSchemaParams::default()));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["agents_omitted"].as_bool(), Some(true));
        assert!(body.get("agents").is_none());
        assert!(body.get("consultants").is_none());
        assert!(body.get("text").is_none());
        assert!(
            body["agents_hint"]
                .as_str()
                .unwrap()
                .contains("include_agents")
        );
    }

    #[test]
    fn bbox_describe_schema_includes_installed_agents_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let cat = server.state.artifacts.read();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "schema-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "schema-tester",
                "version": 1,
                "manifest": {
                    "description": "Agent for schema test.",
                    "when_to_use": ["use when testing schema"],
                    "anti_patterns": ["do not use in prod"],
                    "brofile_inline": {"provider": "claude"},
                    "cost_class": "normal",
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        cat.install_value(
            artifacts::ArtifactKind::Agent,
            "badgey-agent.json".into(),
            &serde_json::json!({
                "kind": "agent",
                "name": "badgey-agent",
                "version": 3,
                "manifest": {
                    "description": "Badgey-backed agent.",
                    "brofile_inline": {"provider": "claude"},
                    "cost_class": "cheap",
                    "dispatch_adapter": "badgey",
                },
            }),
            None,
            None,
            None,
        )
        .unwrap();
        drop(cat);

        let result = server.bbox_describe_schema(Parameters(DescribeSchemaParams {
            include_agents: Some(true),
            mode: None,
            ..Default::default()
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert!(body.get("agents_omitted").is_none());
        let agents = body["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 1);
        let schema_tester = agents
            .iter()
            .find(|a| a["name"] == "schema-tester")
            .unwrap();
        assert_eq!(schema_tester["version"].as_str(), Some("1"));
        assert_eq!(schema_tester["cost_class"].as_str(), Some("normal"));
        assert_eq!(schema_tester["when_to_use"].as_array().unwrap().len(), 1);
        assert_eq!(schema_tester["anti_patterns"].as_array().unwrap().len(), 1);
        assert!(schema_tester["dispatch_adapter"].is_null());

        assert!(!agents.iter().any(|agent| agent["name"] == "badgey-agent"));

        assert!(body.get("agents_by_dispatch_adapter").is_none());
    }

    /// gap-edc84378: transcript entities are deliberately excluded from
    /// EdgeIndex's active counts, so bbox_describe_schema's transcript
    /// population_count must come from a tantivy doc_type count instead. This
    /// used to fall through to 0 for every caller.
    #[test]
    fn bbox_describe_schema_reports_transcript_count_from_tantivy() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        {
            let idx = server.state.idx.write();
            let fields = idx.field_handles();
            let mut writer = idx.index_handle().writer(50_000_000).unwrap();
            let mut transcript = tantivy::TantivyDocument::new();
            transcript.add_text(fields.doc_type, "transcript");
            transcript.add_text(fields.account, "claude");
            transcript.add_text(fields.session_id, "sess-1");
            transcript.add_u64(fields.byte_offset, 0);
            writer.add_document(transcript).unwrap();
            writer.commit().unwrap();
            idx.reader_reload_for_test();
        }

        let result = server.bbox_describe_schema(Parameters(DescribeSchemaParams::default()));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        let vertex_types = body["vertex_types"].as_array().unwrap();
        let transcript_vertex = vertex_types
            .iter()
            .find(|v| v["entity_type"] == "transcript")
            .expect("transcript vertex type present");
        assert!(
            transcript_vertex["population_count"].as_u64().unwrap() >= 1,
            "expected transcript population_count >= 1, got {transcript_vertex}"
        );
    }

    /// gap-edc84378 fold: bbox_inspect_entity used to 404 real transcript
    /// refs hybrid_search had just returned, because
    /// TranscriptIndex::transcript_properties capped its per-session doc
    /// scan at a fixed size (fixed in bbox-corpus-index, see its doc
    /// comment). A transcript ref with a matching tantivy doc must inspect
    /// OK and carry the synthesized IN_SESSION out-edge; one with no
    /// matching doc must still 404 -- the eval oracle depends on genuinely
    /// dead refs staying dead.
    #[tokio::test]
    async fn bbox_inspect_entity_resolves_transcript_doc_and_synthesizes_in_session() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        {
            let idx = server.state.idx.write();
            let fields = idx.field_handles();
            let mut writer = idx.index_handle().writer(50_000_000).unwrap();
            let mut transcript = tantivy::TantivyDocument::new();
            transcript.add_text(fields.doc_type, "transcript");
            transcript.add_text(fields.account, "claude");
            transcript.add_text(fields.session_id, "sess-1");
            transcript.add_u64(fields.byte_offset, 42);
            transcript.add_text(fields.role, "assistant");
            writer.add_document(transcript).unwrap();
            writer.commit().unwrap();
            idx.reader_reload_for_test();
        }

        let inspect = |entity_ref: String| {
            let server = server.clone();
            async move {
                let result = server
                    .bbox_inspect_entity(Parameters(InspectEntityParams {
                        edge_cursor: None,
                        property: None,
                        property_cursor: None,
                        property_limit: None,
                        entity_ref,
                        provisional: None,
                        edge_types: None,
                        direction: None,
                        per_type_limit: Some(5),
                        property_mode: Some("full".into()),
                    }))
                    .await;
                serde_json::from_str::<serde_json::Value>(&extract_text(&result)).unwrap()
            }
        };

        let found = inspect("transcript:claude:sess-1:42:0".to_string()).await;
        assert_eq!(found["status"], "ok", "expected ok, got {found}");
        assert_eq!(found["properties"]["role"], "assistant");
        let out_edges = found["edges"]["out"].as_array().unwrap();
        assert!(
            out_edges
                .iter()
                .any(|edge| edge["kind"] == "IN_SESSION"
                    && edge["target"] == "session:claude:sess-1"),
            "expected synthesized IN_SESSION out-edge, got {out_edges:?}"
        );

        // A ref whose (session, byte_offset) matches no doc must stay
        // not_found -- do not fabricate entities for arbitrary offsets.
        let missing = inspect("transcript:claude:sess-1:999:0".to_string()).await;
        assert_eq!(missing["status"], "error.not_found");
    }

    #[tokio::test]
    async fn provenance_export_plan_requires_authoritative_session_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let result = server
            .bbox_provenance_export_plan(Parameters(ProvenanceExportPlanParams::default()))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(extract_text(&result).contains("error.no_authoritative_checkout"));
    }

    #[tokio::test]
    async fn deferred_edge_index_refuses_graph_reads_until_complete_view_is_published() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        server
            .state
            .edge_index_ready
            .store(false, std::sync::atomic::Ordering::Release);

        let result = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                edge_cursor: None,
                property: None,
                property_cursor: None,
                property_limit: None,
                entity_ref: "thread:thread-00000000".into(),
                provisional: None,
                edge_types: None,
                direction: None,
                per_type_limit: Some(5),
                property_mode: Some("summary".into()),
            }))
            .await;

        assert_eq!(result.is_error, Some(true));
        assert!(extract_text(&result).contains("error.edge_index_warming"));

        let schema = server.bbox_describe_schema(Parameters(DescribeSchemaParams::default()));
        assert_eq!(schema.is_error, Some(true));
        assert!(extract_text(&schema).contains("error.edge_index_warming"));

        let ref_size = server
            .bbox_ref_size(Parameters(RefSizeParams {
                refs: Vec::new(),
                project_dir: None,
                ..Default::default()
            }))
            .await;
        assert_eq!(ref_size.is_error, Some(true));
        assert!(extract_text(&ref_size).contains("error.edge_index_warming"));
    }

    #[tokio::test]
    async fn provenance_export_plan_accepts_scope_bound_operator_authority() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&root)
            .unwrap();
        let scope = PublishedScope::try_new("repo", ".").unwrap();
        server
            .session_operator_provenance_binding
            .set(Some(std::sync::Arc::new(
                crate::server::provenance_authority::OperatorProvenanceGrant {
                    project_id: project.project_id.clone(),
                    scope: scope.clone(),
                },
            )))
            .unwrap();

        let result = server
            .bbox_provenance_export_plan(Parameters(ProvenanceExportPlanParams::default()))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", extract_text(&result));
        let page: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(page["project_id"], project.project_id);
        assert_eq!(page["scope"]["repo_id"], scope.repo_id());
        assert_eq!(
            page["scope"]["bbox_root_relpath"],
            scope.bbox_root_relpath()
        );
    }

    #[tokio::test]
    async fn provenance_export_plan_requires_session_project_to_remain_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        server.set_session_checkout_for_test(
            "unregistered-project".into(),
            bbox_corpus_core::identity::PublishedScope::try_new("repo", ".").unwrap(),
            "checkout".into(),
            root,
        );
        let result = server
            .bbox_provenance_export_plan(Parameters(ProvenanceExportPlanParams::default()))
            .await;
        assert_eq!(result.is_error, Some(true));
        assert!(extract_text(&result).contains("error.project_not_registered"));
    }
}

/// Catalog-mode adapter tests (plan section 13.5).
///
/// The shared `catalog_fixture` in `src/server/state.rs` builds catalog state
/// for the published-read milestones and installs `DenyCheckoutAccess`, which
/// is exactly wrong for adapters whose whole subject is which lease they take.
/// This scaffold instead runs the real catalog checkout authority over real
/// checkouts so capability, identity, and revalidation refusals are the
/// authority's, not a stub's.
#[cfg(test)]
mod catalog_adapter_tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::{
        AttachmentCapabilities, AttachmentId, AttachmentKind, AttachmentStatus, CheckoutAttachment,
        CommitNamespace, CorpusProject, ProjectId, ProjectScope, RecordedRepoAuthority,
        RepoHistoryAuthority, RepoHistoryId, RepoHistoryMaterialization, RepoHistoryRecord,
    };
    use bbox_indexing::blame_locality_cutover::{
        BlameLocalityCutoverMarkerV1, BlameLocalityCutoverRowV1, BlameLocalityCutoverRuntimeV1,
    };
    use bbox_indexing::blame_locality_observations::{
        BlameLocalityComparisonV1, BlameLocalityTargetV1,
    };
    use bbox_indexing::checkout_access::{
        CheckoutAccessBroker, CheckoutAccessIntent, CheckoutAccessKind,
    };
    use bbox_indexing::git_transport_cutover::{
        GitTransportCutoverMarkerV1, GitTransportCutoverRuntimeV1,
        PredictedGitTransportCutoverRowV1,
    };
    use bbox_indexing::project_catalog_inventory::Sha256ValueV1;
    use bbox_indexing::project_catalog_store::ProjectCatalogStore;

    use super::*;
    use crate::server::state::SharedState;

    const PROJECT_ONE: &str = "p_000000000000000000000000000000a1";
    const PROJECT_TWO: &str = "p_000000000000000000000000000000b1";
    const ATTACHMENT_ONE: &str = "att_00000000000000000000000000000a01";
    const ATTACHMENT_TWO: &str = "att_00000000000000000000000000000a02";

    struct CatalogAdapters {
        _directory: tempfile::TempDir,
        root: PathBuf,
        catalog_path: PathBuf,
        store: Arc<ProjectCatalogStore>,
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    /// Everything an attachment row needs that is not derivable from its
    /// checkout. Spelled out per test so a capability or lifecycle refusal is
    /// visibly the row's, never a default's.
    struct AttachSpec<'a> {
        project_id: &'a str,
        attachment_id: &'a str,
        dir_name: &'a str,
        kind: AttachmentKind,
        status: AttachmentStatus,
        capabilities: AttachmentCapabilities,
        scope: Option<PublishedScope>,
        default_for_project: bool,
    }

    fn capabilities(kinds: &[CheckoutAccessKind]) -> AttachmentCapabilities {
        let mut capabilities = AttachmentCapabilities::default();
        for kind in kinds {
            match kind {
                CheckoutAccessKind::LocalProjectWalk => capabilities.local_code_source = true,
                CheckoutAccessKind::GitHistory => capabilities.git_history = true,
                CheckoutAccessKind::PublisherConfigTreeRead
                | CheckoutAccessKind::KnowledgeGapOverlayRead => capabilities.repo_knowledge = true,
                CheckoutAccessKind::Blame => capabilities.blame = true,
                CheckoutAccessKind::RenderFileProvider => capabilities.render_output = true,
                CheckoutAccessKind::ProvenanceNoteIo => capabilities.provenance_note_io = true,
                CheckoutAccessKind::ArtifactWatchDiscovery => capabilities.artifact_watching = true,
                CheckoutAccessKind::RepositoryMutation => capabilities.repo_mutation = true,
            }
        }
        capabilities
    }

    impl CatalogAdapters {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let root = directory.path().canonicalize().unwrap();
            let catalog_root = root.join("catalog");
            std::fs::create_dir_all(&catalog_root).unwrap();
            let catalog_path = catalog_root.join("projects.json");
            let store = Arc::new(ProjectCatalogStore::initialize_empty(&catalog_path).unwrap());
            Self {
                _directory: directory,
                root,
                catalog_path,
                store,
            }
        }

        fn scope(repo: &str) -> PublishedScope {
            PublishedScope::try_new(repo, ".").unwrap()
        }

        fn add_project(&self, project_id: &str, scope: Option<PublishedScope>) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |catalog, _attachments| {
                    catalog.projects.insert(
                        project_id.clone(),
                        CorpusProject {
                            project_id: project_id.clone(),
                            scope: match &scope {
                                Some(scope) => ProjectScope::Published(scope.clone()),
                                None => ProjectScope::LegacyLocal,
                            },
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

        fn bind_repo_history(&self, project_id: &str, repo_history_id: &str, authority: &str) {
            let project_id = ProjectId::parse(project_id).unwrap();
            let repo_history_id = RepoHistoryId::parse(repo_history_id).unwrap();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |catalog, _attachments| {
                    catalog.repo_histories.insert(
                        repo_history_id.clone(),
                        RepoHistoryRecord {
                            repo_history_id: repo_history_id.clone(),
                            membership_generation: 0,
                            authority: RepoHistoryAuthority::Recorded(
                                RecordedRepoAuthority::parse(authority).unwrap(),
                            ),
                            primary_namespace: CommitNamespace::parse(authority).unwrap(),
                            compatibility_namespaces: Default::default(),
                            materialization: RepoHistoryMaterialization::NotBuilt,
                        },
                    );
                    catalog.projects.get_mut(&project_id).unwrap().repo_history =
                        Some(repo_history_id.clone());
                    Ok(())
                })
                .unwrap();
        }

        /// Materialize the checkout, mint its durable identity marker, and
        /// record the attachment. The marker is minted rather than invented:
        /// the catalog authority verifies it on every acquisition.
        fn attach(&self, spec: AttachSpec<'_>) -> PathBuf {
            let checkout_dir = self.root.join(spec.dir_name);
            std::fs::create_dir_all(&checkout_dir).unwrap();
            let checkout_dir = checkout_dir.canonicalize().unwrap();
            let checkout_id =
                bbox_corpus_core::identity::ensure_checkout_id(&checkout_dir).unwrap();
            let project_id = ProjectId::parse(spec.project_id).unwrap();
            let attachment_id = AttachmentId::parse(spec.attachment_id).unwrap();
            let dir = checkout_dir.to_string_lossy().into_owned();
            let epoch = self.store.snapshot().unwrap().epoch();
            self.store
                .transact(epoch, |_catalog, attachments| {
                    attachments.attachments.insert(
                        attachment_id.clone(),
                        CheckoutAttachment {
                            attachment_id: attachment_id.clone(),
                            project_id: project_id.clone(),
                            checkout_id: checkout_id.clone(),
                            checkout_dir: dir.clone(),
                            checkout_project_dir: dir.clone(),
                            project_root_relpath: ".".into(),
                            kind: spec.kind,
                            validated_scope: spec.scope.clone(),
                            computed_repo_hint: None,
                            branch_ref: Some("refs/heads/main".into()),
                            capabilities: spec.capabilities,
                            status: spec.status,
                            attached_at: "2026-08-03T00:00:00Z".into(),
                            detached_at: match spec.status {
                                AttachmentStatus::Attached => None,
                                _ => Some("2026-08-03T00:00:01Z".into()),
                            },
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
            checkout_dir
        }

        /// A catalog-authority server over the same durable bytes, with the
        /// REAL catalog checkout authority rather than the deny stub.
        fn server(&self) -> BlackboxServer {
            self.server_with_cutover(GitTransportCutoverRuntimeV1::default())
        }

        fn server_with_cutover(&self, cutover: GitTransportCutoverRuntimeV1) -> BlackboxServer {
            self.server_with_cutovers(cutover, BlameLocalityCutoverRuntimeV1::default())
        }

        fn server_with_blame_cutover(
            &self,
            cutover: BlameLocalityCutoverRuntimeV1,
        ) -> BlackboxServer {
            self.server_with_cutovers(GitTransportCutoverRuntimeV1::default(), cutover)
        }

        fn server_with_cutovers(
            &self,
            git_cutover: GitTransportCutoverRuntimeV1,
            blame_cutover: BlameLocalityCutoverRuntimeV1,
        ) -> BlackboxServer {
            let mut state = SharedState::for_test_catalog(&self.root, &self.catalog_path);
            let store = state
                .project_authority
                .catalog_store()
                .expect("catalog authority")
                .clone();
            state.checkout_access = Arc::new(CheckoutAccessBroker::new(
                Arc::new(
                    bbox_indexing::checkout_access_v2::V2CatalogCheckoutAccessAuthority::new(store),
                ),
                state.checkout_access_observations.clone(),
            ));
            state.git_transport_cutover = Arc::new(git_cutover);
            state.blame_locality_cutover = Arc::new(blame_cutover);
            BlackboxServer::new(Arc::new(state))
        }
    }

    fn blame_cutover_runtime(
        project_id: &str,
        scope: &PublishedScope,
    ) -> BlameLocalityCutoverRuntimeV1 {
        let comparison = |target| BlameLocalityComparisonV1 {
            project_id: project_id.into(),
            target,
            local_response_sha256: "a".repeat(64),
            legacy_response_sha256: "a".repeat(64),
            equal: true,
            sequence: 1,
            observed_at_unix_secs: 1,
        };
        let mut marker = BlameLocalityCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:1".into(),
            report_sha256: "b".repeat(64),
            catalog_epoch: 1,
            catalog_sha256: "c".repeat(64),
            rows: vec![BlameLocalityCutoverRowV1 {
                project_id: ProjectId::parse(project_id).unwrap(),
                scope: scope.clone(),
                producer_id: "producer-a".into(),
                path_comparison: comparison(BlameLocalityTargetV1::Path),
                entity_comparison: comparison(BlameLocalityTargetV1::Entity),
                checkout_baselines: Vec::new(),
            }],
            checksum_sha256: String::new(),
        };
        marker.checksum_sha256 = hex::encode(Sha256::digest(
            serde_json::to_vec(&(
                marker.version,
                &marker.applied_at,
                &marker.report_sha256,
                marker.catalog_epoch,
                &marker.catalog_sha256,
                &marker.rows,
            ))
            .unwrap(),
        ));
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(
            directory
                .path()
                .join(bbox_indexing::blame_locality_cutover::BLAME_LOCALITY_CUTOVER_MARKER_FILE),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        BlameLocalityCutoverRuntimeV1::open(directory.path()).unwrap()
    }

    fn blame_spec<'a>(project_id: &'a str, attachment_id: &'a str, dir: &'a str) -> AttachSpec<'a> {
        AttachSpec {
            project_id,
            attachment_id,
            dir_name: dir,
            kind: AttachmentKind::Base,
            status: AttachmentStatus::Attached,
            capabilities: capabilities(&[CheckoutAccessKind::Blame]),
            scope: Some(CatalogAdapters::scope("repo-one")),
            default_for_project: false,
        }
    }

    fn acquire_blame(server: &BlackboxServer, project_id: &str) -> Result<ValidatedCheckoutLease> {
        acquire_selected_operation(
            server,
            &server.state.checkout_access.clone(),
            project_id,
            CheckoutAccessKind::Blame,
            CheckoutAccessIntent::Read,
        )
    }

    /// The single active attachment is selected, natively, with the catalog's
    /// own scope and with no `PublisherConfigTreeRead` lease in front of it.
    #[test]
    fn single_active_attachment_is_selected_natively_without_a_scope_lease() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let server = fixture.server();

        // The fixture grants `blame` and NOTHING else: no `repo_knowledge`.
        // Succeeding here is half the capability contract, and
        // `blame_is_denied_on_its_own_capability_not_repo_knowledge` is the
        // other half.
        let lease = acquire_blame(&server, PROJECT_ONE).unwrap();

        assert_eq!(lease.attachment_id(), ATTACHMENT_ONE);
        assert_eq!(lease.published_scope(), Some(&scope));
        assert_eq!(lease.kind(), CheckoutAccessKind::Blame);
        assert_eq!(
            lease.source_lane(),
            CheckoutAccessSourceLane::NativeAttachment
        );
        // The publisher-config capability was never requested, so a project
        // whose attachment lacks `repo_knowledge` still blames.
        let publisher_operations = server
            .state
            .checkout_access
            .health()
            .operations
            .into_iter()
            .find(|operation| operation.kind == CheckoutAccessKind::PublisherConfigTreeRead)
            .map(|operation| operation.granted + operation.denied)
            .unwrap_or_default();
        assert_eq!(publisher_operations, 0);
    }

    /// The operator-selected default wins over the other active attachments.
    #[test]
    fn operator_default_attachment_wins_over_other_active_attachments() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let mut second = blame_spec(PROJECT_ONE, ATTACHMENT_TWO, "checkout-two");
        second.kind = AttachmentKind::Worktree;
        second.default_for_project = true;
        fixture.attach(second);
        let server = fixture.server();

        let lease = acquire_blame(&server, PROJECT_ONE).unwrap();

        assert_eq!(lease.attachment_id(), ATTACHMENT_TWO);
    }

    /// A session-pinned checkout outranks the operator default: the request
    /// runs where the caller is, not where the host prefers.
    #[test]
    fn session_pinned_checkout_outranks_the_operator_default() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        let mut default = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        default.default_for_project = true;
        fixture.attach(default);
        let mut pinned = blame_spec(PROJECT_ONE, ATTACHMENT_TWO, "checkout-two");
        pinned.kind = AttachmentKind::Worktree;
        let pinned_dir = fixture.attach(pinned);
        let checkout_id = bbox_corpus_core::identity::ensure_checkout_id(&pinned_dir).unwrap();
        let server = fixture.server();
        server
            .session_checkout
            .set(Some(Arc::new(
                bbox_corpus_core::project_record::ResolvedCheckoutScope {
                    project_id: PROJECT_ONE.into(),
                    published_scope: scope.clone(),
                    checkout_id,
                    checkout_dir: pinned_dir.to_string_lossy().into_owned(),
                    checkout_project_dir: pinned_dir.to_string_lossy().into_owned(),
                    branch_ref: Some("refs/heads/main".into()),
                },
            )))
            .unwrap();

        let lease = acquire_blame(&server, PROJECT_ONE).unwrap();

        assert_eq!(lease.attachment_id(), ATTACHMENT_TWO);
    }

    /// D-033 item 3's last rung: with two active attachments, no session pin
    /// and no operator default, the unique active `Base` decides.
    #[test]
    fn unique_active_base_resolves_an_otherwise_ambiguous_project() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let mut worktree = blame_spec(PROJECT_ONE, ATTACHMENT_TWO, "checkout-two");
        worktree.kind = AttachmentKind::Worktree;
        fixture.attach(worktree);
        let server = fixture.server();

        let lease = acquire_blame(&server, PROJECT_ONE).unwrap();

        assert_eq!(lease.attachment_id(), ATTACHMENT_ONE);
    }

    /// Two active bases have no unique base rung left, so the ambiguity the
    /// resolver reported stands rather than being silently broken.
    #[test]
    fn two_active_bases_stay_ambiguous() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_TWO, "checkout-two"));
        let server = fixture.server();

        let error = acquire_blame(&server, PROJECT_ONE).unwrap_err().to_string();

        assert!(
            error.starts_with("error.project_attachment_ambiguous"),
            "{error}"
        );
    }

    /// A remote-only catalog project degrades to attachment-required, not to
    /// project-not-found (plan section 10.5).
    #[test]
    fn remote_only_project_requires_an_attachment() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let server = fixture.server();

        let error = acquire_blame(&server, PROJECT_ONE).unwrap_err().to_string();

        assert!(
            error.starts_with("error.project_attachment_required"),
            "{error}"
        );
    }

    /// A detached attachment is not an attachment: the row exists, and the
    /// refusal is still attachment-required rather than a lease denial.
    #[test]
    fn detached_attachment_requires_an_attachment() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let mut detached = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        detached.status = AttachmentStatus::Detached;
        detached.capabilities = AttachmentCapabilities::default();
        fixture.attach(detached);
        let server = fixture.server();

        let error = acquire_blame(&server, PROJECT_ONE).unwrap_err().to_string();

        assert!(
            error.starts_with("error.project_attachment_required"),
            "{error}"
        );
    }

    /// The denial names the capability the OPERATION needs. An attachment
    /// carrying `repo_knowledge` but not `blame` must still be denied, which
    /// is the asymmetry a scope-discovery lease would have inverted.
    #[test]
    fn blame_is_denied_on_its_own_capability_not_repo_knowledge() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let mut knowledge_only = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        knowledge_only.capabilities = capabilities(&[CheckoutAccessKind::PublisherConfigTreeRead]);
        fixture.attach(knowledge_only);
        let server = fixture.server();

        let error = acquire_blame(&server, PROJECT_ONE).unwrap_err().to_string();

        assert!(
            error.starts_with("error.checkout_access.capability_denied"),
            "{error}"
        );
    }

    /// Scope disagreement never reaches an adapter: the request pins the
    /// catalog project's own scope, and the pair store refuses to record an
    /// attachment validated at a different one. The adapter's `expected_scope`
    /// pin is the second line of defense behind that, not the first.
    #[test]
    fn attachment_scope_disagreement_cannot_be_recorded() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let mut foreign = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        foreign.scope = Some(CatalogAdapters::scope("repo-other"));

        let refusal = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fixture.attach(foreign);
        }))
        .unwrap_err();

        let message = refusal
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_default();
        assert!(
            message.contains("error.project_attachments_scope_mismatch"),
            "{message}"
        );
    }

    /// Identity is the marker, not the path: a checkout whose marker names a
    /// different checkout denies even though the directory still exists.
    #[test]
    fn checkout_identity_marker_divergence_is_refused() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        std::fs::write(
            checkout.join(".bbox/local/checkout-id"),
            "ffffffffffffffffffffffffffffffff",
        )
        .unwrap();
        let server = fixture.server();

        let error = acquire_blame(&server, PROJECT_ONE).unwrap_err().to_string();

        assert!(
            error.starts_with("error.checkout_access.checkout_identity_mismatch"),
            "{error}"
        );
    }

    /// Revalidation is a real recheck, not a formality: identity lost after
    /// acquisition fails the operation rather than blessing the read.
    #[test]
    fn revalidation_fails_when_identity_is_lost_after_acquisition() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let server = fixture.server();
        let lease = acquire_blame(&server, PROJECT_ONE).unwrap();

        std::fs::remove_file(checkout.join(".bbox/local/checkout-id")).unwrap();
        let error = server
            .state
            .checkout_access
            .revalidate(&lease)
            .unwrap_err()
            .to_string();

        assert!(error.contains("checkout_identity_mismatch"), "{error}");
    }

    /// An absolute selector resolves through the catalog's active attachments
    /// and strips the LEASE root. No `ProjectRecord::canonical_path`
    /// participates: the project here has no compatibility row at all,
    /// because its only attachment is a worktree.
    #[test]
    fn absolute_selection_resolves_without_a_compatibility_record() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let mut worktree = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        worktree.kind = AttachmentKind::Worktree;
        worktree.capabilities = capabilities(&[CheckoutAccessKind::RenderFileProvider]);
        let checkout = fixture.attach(worktree);
        std::fs::write(checkout.join("file.rs"), "fn main() {}\n").unwrap();
        let server = fixture.server();
        assert!(
            server
                .state
                .records_provider
                .records_snapshot()
                .records
                .is_empty(),
            "a worktree-only project has no compatibility row to match"
        );

        let selection = file_selection(
            &server,
            &server.state.checkout_access.clone(),
            checkout.join("file.rs").to_str().unwrap(),
            None,
            None,
            &[],
            &[],
        )
        .unwrap();
        let acquired = acquire_file_selection(
            &server,
            &server.state.checkout_access.clone(),
            selection,
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap();

        assert_eq!(acquired.relative_path, "file.rs");
        assert_eq!(acquired.content, b"fn main() {}\n");
    }

    /// A path outside every active attachment is refused; nothing falls back
    /// to a registered root.
    #[test]
    fn absolute_selection_outside_every_attachment_is_refused() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let stranger = fixture.root.join("stranger");
        std::fs::create_dir_all(&stranger).unwrap();
        std::fs::write(stranger.join("file.rs"), "fn main() {}\n").unwrap();
        let server = fixture.server();

        let selection = file_selection(
            &server,
            &server.state.checkout_access.clone(),
            stranger.join("file.rs").to_str().unwrap(),
            None,
            None,
            &[],
            &[],
        )
        .unwrap();
        let error = acquire_file_selection(
            &server,
            &server.state.checkout_access.clone(),
            selection,
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.starts_with("error.checkout_access.attachment_not_found"),
            "{error}"
        );
    }

    /// Traversal is refused before any authority is consulted.
    #[test]
    fn traversal_selectors_are_refused_before_acquisition() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let server = fixture.server();

        for selector in ["../escape.rs", "nested/../../escape.rs"] {
            let error = catalog_file_selection(&server, selector, None, None)
                .unwrap_err()
                .to_string();
            assert!(
                error.starts_with("error.checkout_path_invalid"),
                "{selector}: {error}"
            );
        }
    }

    /// A relative read reaching outside the attachment through a symlink is
    /// refused by the lease's own path gate, after acquisition.
    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_refused_by_the_lease_path_gate() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let mut render = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        render.capabilities = capabilities(&[CheckoutAccessKind::RenderFileProvider]);
        let checkout = fixture.attach(render);
        let outside = fixture.root.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(&outside, checkout.join("escape")).unwrap();
        let server = fixture.server();

        let selection = catalog_file_selection(&server, "escape/secret.txt", None, None).unwrap();
        let error = acquire_file_selection(
            &server,
            &server.state.checkout_access.clone(),
            selection,
            CheckoutAccessKind::RenderFileProvider,
            CheckoutAccessIntent::Read,
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.starts_with("error.checkout_access.conservative_path_gate_denied"),
            "{error}"
        );
    }

    /// A relative read with no session pin and more than one attached project
    /// refuses rather than picking one.
    #[test]
    fn relative_read_without_a_session_refuses_when_more_than_one_project_is_attached() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        fixture.add_project(PROJECT_TWO, Some(CatalogAdapters::scope("repo-two")));
        fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let mut second = blame_spec(PROJECT_TWO, ATTACHMENT_TWO, "checkout-two");
        second.scope = Some(CatalogAdapters::scope("repo-two"));
        fixture.attach(second);
        let server = fixture.server();

        let error = catalog_file_selection(&server, "file.rs", None, None)
            .unwrap_err()
            .to_string();

        assert!(
            error.starts_with("error.project_selector_ambiguous"),
            "{error}"
        );
    }

    /// A host with catalog projects but no attachment at all reports the
    /// attachment requirement, not an empty registry.
    #[test]
    fn relative_read_without_any_attachment_requires_one() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let server = fixture.server();

        let error = catalog_file_selection(&server, "file.rs", None, None)
            .unwrap_err()
            .to_string();

        assert!(
            error.starts_with("error.project_attachment_required"),
            "{error}"
        );
    }

    /// A legacy all-project Git-note operation covers the COMPLETE catalog
    /// set. A remote-only peer therefore fails the whole call at the first
    /// typed refusal instead of being skipped (plan section 4.20).
    #[test]
    fn legacy_provenance_covers_every_catalog_project_and_is_never_partial() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        fixture.add_project(PROJECT_TWO, Some(CatalogAdapters::scope("repo-two")));
        let mut notes = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        notes.capabilities = capabilities(&[CheckoutAccessKind::ProvenanceNoteIo]);
        fixture.attach(notes);
        let server = fixture.server();
        let params = ProvenanceParams { project_id: None };

        let requested = requested_provenance_projects(&server, &params, &[]).unwrap();
        assert_eq!(requested.len(), 2, "the remote-only peer is not dropped");

        let error = acquire_provenance_projects(
            &server,
            &server.state.checkout_access.clone(),
            &params,
            &[],
            CheckoutAccessIntent::Read,
        )
        .unwrap_err()
        .to_string();

        assert!(
            error.starts_with("error.project_attachment_required"),
            "{error}"
        );
    }

    #[test]
    fn covered_provenance_refuses_before_the_first_checkout_lease() {
        const REPO_HISTORY: &str = "rh_000000000000000000000000000000a1";
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        fixture.bind_repo_history(PROJECT_ONE, REPO_HISTORY, "repo-one");
        let mut notes = blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one");
        notes.capabilities = capabilities(&[CheckoutAccessKind::ProvenanceNoteIo]);
        fixture.attach(notes);

        let repo_history_id = RepoHistoryId::parse(REPO_HISTORY).unwrap();
        let catalog = fixture.store.snapshot().unwrap();
        let membership_generation =
            catalog.catalog().repo_histories[&repo_history_id].membership_generation;
        let marker = GitTransportCutoverMarkerV1 {
            version: 1,
            applied_at: "unix:2".to_string(),
            report_artifact_hash: Sha256ValueV1::digest(b"report"),
            resolution_artifact_hash: Sha256ValueV1::digest(b"resolution"),
            predecessor_marker_checksum: None,
            predecessor_catalog_epoch: catalog.epoch(),
            inventory_hash: Sha256ValueV1::digest(b"inventory"),
            aggregate_grant_hash: Sha256ValueV1::digest(b"grants"),
            zero_prepared_history_journals: true,
            zero_prepared_provenance_journals: true,
            rows: vec![PredictedGitTransportCutoverRowV1 {
                repo_history_id,
                grant_commitment: "d".repeat(64),
                membership_generation,
                source_generation_id: "source-one".to_string(),
                p3_generation_id: format!("rhg_{}", "a".repeat(64)),
                history_parity_commitment: Sha256ValueV1::digest(b"history"),
                provenance_import_generations: Default::default(),
                provenance_export_generations: Default::default(),
                provenance_parity_commitments: Default::default(),
                capability_baselines: Vec::new(),
            }],
            checksum_sha256: Sha256ValueV1::digest(b"checksum"),
        };
        drop(catalog);
        let server =
            fixture.server_with_cutover(GitTransportCutoverRuntimeV1::from_marker(Some(marker)));

        let error = acquire_provenance_projects(
            &server,
            &server.state.checkout_access.clone(),
            &ProvenanceParams {
                project_id: Some(PROJECT_ONE.to_string()),
            },
            &[],
            CheckoutAccessIntent::Read,
        )
        .unwrap_err()
        .to_string();
        assert!(
            error.starts_with("error.provenance_transport_authoritative"),
            "{error}"
        );

        let health = server.state.checkout_access.health();
        let provenance = health
            .operations
            .iter()
            .find(|operation| operation.kind == CheckoutAccessKind::ProvenanceNoteIo)
            .unwrap();
        assert_eq!(provenance.granted, 0);
        assert_eq!(provenance.denied, 0);
        assert!(
            health.target_counters.iter().all(|counter| {
                counter.kind != CheckoutAccessKind::ProvenanceNoteIo || counter.count == 0
            }),
            "the governed request must be refused before the first target lease observation"
        );
    }

    #[tokio::test]
    async fn covered_blame_refuses_path_and_entity_before_checkout_access() {
        let fixture = CatalogAdapters::new();
        let scope = CatalogAdapters::scope("repo-one");
        fixture.add_project(PROJECT_ONE, Some(scope.clone()));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "t"]);
        std::fs::write(checkout.join("file.rs"), "fn main() {}\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "seed"]);

        let governed =
            fixture.server_with_blame_cutover(blame_cutover_runtime(PROJECT_ONE, &scope));
        governed
            .surface_project
            .set(Some(Arc::from(PROJECT_ONE)))
            .unwrap();
        let before = governed.state.checkout_access.health().sequence;
        let path = governed
            .bbox_blame(Parameters(BlameParams {
                file: Some("file.rs".into()),
                line: Some(1),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_eq!(path.is_error, Some(true));
        assert!(extract_text(&path).contains("error.blame_locality_required"));
        let entity = mcp_tools::blame::BlameTargetIdentity::ProjectFile {
            project_id: PROJECT_ONE.into(),
            indexed_path_hint: PathBuf::from("file.rs"),
            line: Some(1),
            byte_offset: 0,
        };
        let entity_error = enforce_blame_locality_cutover(&governed, &entity)
            .unwrap_err()
            .to_string();
        assert!(entity_error.contains("error.blame_locality_required"));
        assert_eq!(governed.state.checkout_access.health().sequence, before);

        let uncovered = fixture.server();
        uncovered
            .surface_project
            .set(Some(Arc::from(PROJECT_ONE)))
            .unwrap();
        let before = uncovered.state.checkout_access.health().sequence;
        let result = uncovered
            .bbox_blame(Parameters(BlameParams {
                file: Some("file.rs".into()),
                line: Some(1),
                entity_ref: None,
                locality: None,
            }))
            .await;
        assert_ne!(result.is_error, Some(true), "{}", extract_text(&result));
        assert!(uncovered.state.checkout_access.health().sequence > before);
    }

    /// Corpus-identity blame refuses an absolute indexed hint in catalog
    /// mode: catalog identity carries no record root, and no attachment root
    /// may stand in for one.
    #[test]
    fn absolute_indexed_hint_is_refused_under_catalog_identity() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        let server = fixture.server();

        let error = acquire_project_file(
            &server,
            &server.state.checkout_access.clone(),
            PROJECT_ONE,
            &checkout.join("src/lib.rs"),
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(error.starts_with("error.indexed_path_mismatch"), "{error}");
    }

    fn git(dir: &Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(args)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed in {dir:?}");
    }

    fn overlay_map(
        project_id: &str,
        repo_head: &str,
    ) -> std::collections::BTreeMap<String, bbox_corpus_core::git_overlay::GitOverlaySelector> {
        std::collections::BTreeMap::from([(
            project_id.to_string(),
            bbox_corpus_core::git_overlay::GitOverlaySelector {
                project_id: project_id.to_string(),
                code_generation: "cg_test".into(),
                repo_history_generation: "rhg_test".into(),
                source: bbox_corpus_core::git_overlay::GitOverlaySourceV1::Attachment {
                    attachment_id: ATTACHMENT_ONE.into(),
                },
                repo_head: repo_head.to_string(),
                commit_namespace: "ns".into(),
                overlay_generation: 1,
            },
        )])
    }

    /// The commit-selection contract, in the three shapes that matter.
    ///
    /// The rejected earlier version of this check proved only that the
    /// snapshot commit EXISTED and then blamed the working tree anyway, and
    /// it treated a missing overlay as permission to proceed. Both are
    /// refusals now: without evidence there is no snapshot to be faithful to,
    /// and a checkout that cannot produce the commit cannot answer for it.
    #[test]
    fn corpus_blame_commit_selection_requires_evidence_and_containment() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "t"]);
        std::fs::write(checkout.join("file.rs"), "fn main() {}\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "seed"]);
        let head = bbox_corpus_core::git::current_head(&checkout).unwrap();

        // Evidence present and contained: blame is bound to that exact commit.
        let selected =
            snapshot_commit_for_blame(&overlay_map(PROJECT_ONE, &head), PROJECT_ONE, &checkout)
                .unwrap();
        assert_eq!(selected, head);

        // Evidence present, commit absent from this checkout.
        let missing = overlay_map(PROJECT_ONE, "0123456789012345678901234567890123456789");
        let error = snapshot_commit_for_blame(&missing, PROJECT_ONE, &checkout)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("error.blame_commit_mismatch"), "{error}");

        // No evidence at all: refuse, never fall through to current history.
        let error =
            snapshot_commit_for_blame(&std::collections::BTreeMap::new(), PROJECT_ONE, &checkout)
                .unwrap_err()
                .to_string();
        assert!(
            error.starts_with("error.blame_snapshot_unavailable"),
            "{error}"
        );
    }

    /// Mutation-verify (a): the checkout has ADVANCED past the snapshot
    /// commit, and the blamed line differs between the two. Corpus-identity
    /// blame must report the snapshot commit's answer.
    ///
    /// This is the direct counterexample the checkpoint raised: proving the
    /// commit exists says nothing, because the working tree can contain a
    /// wholly different line at that number.
    #[test]
    fn advanced_checkout_blames_the_snapshot_commit_not_head() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "t"]);
        std::fs::write(checkout.join("file.rs"), "pub fn indexed() {}\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "the indexed snapshot"]);
        let snapshot = bbox_corpus_core::git::current_head(&checkout).unwrap();

        std::fs::write(checkout.join("file.rs"), "pub fn moved_on() {}\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "the checkout moved on"]);
        let head = bbox_corpus_core::git::current_head(&checkout).unwrap();
        assert_ne!(snapshot, head);

        let edges = bbox_edge_index::edge_index::EdgeIndex::default();
        let snapshot_output = mcp_tools::blame::blame(
            mcp_tools::blame::ValidatedBlameTarget {
                git_root: checkout.clone(),
                git_relative_path: PathBuf::from("file.rs"),
                display_path: "file.rs".into(),
                line: Some(1),
                byte_offset: None,
                source: mcp_tools::blame::BlameSource::Snapshot {
                    commit: snapshot.clone(),
                },
            },
            &edges,
        )
        .unwrap();

        assert!(
            snapshot_output.contains(&snapshot),
            "snapshot blame must attribute the indexed commit: {snapshot_output}"
        );
        assert!(
            !snapshot_output.contains(&head),
            "snapshot blame must not attribute the advanced head: {snapshot_output}"
        );

        // The working-tree arm, which caller-supplied path blame keeps, does
        // report current history. Asserting the contrast is what proves the
        // snapshot arm is doing real work rather than agreeing by accident.
        let working_tree_output = mcp_tools::blame::blame(
            mcp_tools::blame::ValidatedBlameTarget {
                git_root: checkout.clone(),
                git_relative_path: PathBuf::from("file.rs"),
                display_path: "file.rs".into(),
                line: Some(1),
                byte_offset: None,
                source: mcp_tools::blame::BlameSource::WorkingTree {
                    content: b"pub fn moved_on() {}\n".to_vec(),
                },
            },
            &edges,
        )
        .unwrap();
        assert!(working_tree_output.contains(&head), "{working_tree_output}");
    }

    /// A file the corpus indexed may be gone from the working tree. The
    /// snapshot arm still answers, because it reads the committed blob rather
    /// than the checkout.
    #[test]
    fn snapshot_blame_survives_a_file_deleted_from_the_working_tree() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "t"]);
        std::fs::write(checkout.join("file.rs"), "pub fn indexed() {}\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "the indexed snapshot"]);
        let snapshot = bbox_corpus_core::git::current_head(&checkout).unwrap();
        git(&checkout, &["rm", "-q", "file.rs"]);
        git(&checkout, &["commit", "-m", "deleted"]);
        assert!(!checkout.join("file.rs").exists());

        let edges = bbox_edge_index::edge_index::EdgeIndex::default();
        let output = mcp_tools::blame::blame(
            mcp_tools::blame::ValidatedBlameTarget {
                git_root: checkout.clone(),
                git_relative_path: PathBuf::from("file.rs"),
                display_path: "file.rs".into(),
                line: Some(1),
                byte_offset: None,
                source: mcp_tools::blame::BlameSource::Snapshot {
                    commit: snapshot.clone(),
                },
            },
            &edges,
        )
        .unwrap();

        assert!(output.contains(&snapshot), "{output}");
    }

    /// A byte offset resolves against the SNAPSHOT's bytes. Resolving it
    /// against working-tree bytes would silently land on a different line
    /// whenever the file changed length above the target.
    #[test]
    fn byte_offset_resolves_against_snapshot_content() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        let checkout = fixture.attach(blame_spec(PROJECT_ONE, ATTACHMENT_ONE, "checkout-one"));
        git(&checkout, &["init", "--initial-branch", "main"]);
        git(&checkout, &["config", "user.email", "t@example.com"]);
        git(&checkout, &["config", "user.name", "t"]);
        std::fs::write(checkout.join("file.rs"), "one\ntarget\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "snapshot"]);
        let snapshot = bbox_corpus_core::git::current_head(&checkout).unwrap();
        // Prepend two lines: the same byte offset now points elsewhere.
        std::fs::write(checkout.join("file.rs"), "pad\npad\none\ntarget\n").unwrap();
        git(&checkout, &["add", "file.rs"]);
        git(&checkout, &["commit", "-m", "prepended"]);

        let edges = bbox_edge_index::edge_index::EdgeIndex::default();
        let output = mcp_tools::blame::blame(
            mcp_tools::blame::ValidatedBlameTarget {
                git_root: checkout.clone(),
                git_relative_path: PathBuf::from("file.rs"),
                display_path: "file.rs".into(),
                line: None,
                // Offset of "target" in the SNAPSHOT content ("one\n" = 4).
                byte_offset: Some(4),
                source: mcp_tools::blame::BlameSource::Snapshot {
                    commit: snapshot.clone(),
                },
            },
            &edges,
        )
        .unwrap();

        assert!(
            output.contains("\"line\": 2"),
            "byte offset must resolve against snapshot bytes: {output}"
        );
    }

    /// The P5-E residual: a catalog project whose only active attachment is
    /// a WORKTREE is absent from the compatibility projection (that carries
    /// each project's unique active BASE attachment only), so legacy
    /// provenance re-identification refused its anchors even though the
    /// ProvenanceNoteIo lease for that very project had just succeeded.
    ///
    /// Authority is the leased set, so both halves are asserted here: the
    /// projection still omits the project, and the anchor is still
    /// authorized.
    #[test]
    fn legacy_provenance_authorizes_a_worktree_only_catalog_project() {
        let fixture = CatalogAdapters::new();
        fixture.add_project(PROJECT_ONE, Some(CatalogAdapters::scope("repo-one")));
        fixture.attach(AttachSpec {
            project_id: PROJECT_ONE,
            attachment_id: ATTACHMENT_ONE,
            dir_name: "worktree-one",
            kind: AttachmentKind::Worktree,
            status: AttachmentStatus::Attached,
            capabilities: capabilities(&[CheckoutAccessKind::ProvenanceNoteIo]),
            scope: Some(CatalogAdapters::scope("repo-one")),
            default_for_project: false,
        });
        let server = fixture.server();

        let snapshot = server.state.records_provider.records_snapshot();
        assert!(
            snapshot
                .records
                .iter()
                .all(|record| record.project_id != PROJECT_ONE),
            "the cause: the compatibility projection carries base attachments only"
        );
        assert!(
            snapshot.corpus_project_ids.contains(PROJECT_ONE),
            "the project is nonetheless a catalog project"
        );

        let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
        let (leases, inputs) = acquire_provenance_projects(
            &server,
            &broker,
            &ProvenanceParams {
                project_id: Some(PROJECT_ONE.to_string()),
            },
            &snapshot.records,
            CheckoutAccessIntent::Read,
        )
        .expect("the worktree attachment records provenance_note_io");
        assert_eq!(leases.len(), 1);

        let authorized = leased_provenance_projects(&inputs);
        assert!(authorized.contains(PROJECT_ONE));
        authorize_legacy_provenance_target(&authorized, PROJECT_ONE)
            .expect("a leased project's anchors are re-identifiable");

        // An unleased project is still refused: the lease set is authority,
        // not a rubber stamp.
        let error = authorize_legacy_provenance_target(&authorized, PROJECT_TWO)
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("error.project_mismatch"), "{error}");
    }
}

/// The graph vector lane (unified-retrieval design 4.4 / 7.5): a published
/// view install enqueues the composed embed projection of every eligible
/// vertex, a generation flip tombstones the vectors of vertices that left
/// the eligible set, and the describe participation report counts both
/// halves so "why is my graph not in vector search" is answerable without
/// reading a schema artifact.
#[cfg(test)]
mod graph_vector_lane {
    use super::*;
    use crate::server::knowledge_view::{
        PublishedGraphViewInstaller, install_published_graph_view,
    };
    use crate::server::state::SharedState;
    use bbox_corpus_core::identity::PublishedScope;
    use bbox_corpus_core::project_catalog::ProjectId;
    use serde_json::json;
    use std::sync::Arc;

    const GRAPH_ID: &str = "pg-campaigns";

    struct FixedProvider;

    #[async_trait::async_trait]
    impl crate::embed::EmbeddingProvider for FixedProvider {
        async fn embed_batch(
            &self,
            inputs: &[crate::embed::EmbedInput],
            _input_type: crate::embed::EmbedInputType,
        ) -> anyhow::Result<Vec<crate::embed::EmbedOutput>> {
            Ok(inputs
                .iter()
                .map(|_| crate::embed::EmbedOutput::single(vec![1.0, 0.0, 0.0, 0.0]))
                .collect())
        }
        fn dimensions(&self) -> usize {
            4
        }
        fn document_model(&self) -> &str {
            "fixed-test"
        }
        fn endpoint_kind(&self) -> crate::embed::EmbedEndpointKind {
            crate::embed::EmbedEndpointKind::Text
        }
        fn id(&self) -> &str {
            "fixed-test"
        }
    }

    fn install_isolated_graph_queue(root: &std::path::Path) -> Arc<crate::vectors::VectorStore> {
        let vectors = Arc::new(crate::vectors::VectorStore::open(root).unwrap());
        let queue = crate::embed::queue::EmbedQueueHandle::isolated_for_test(
            "graph",
            Arc::new(FixedProvider),
            vectors.clone(),
        );
        crate::embed_queue::install(queue);
        vectors
    }

    fn active_graph_vectors(vectors: &crate::vectors::VectorStore) -> Vec<String> {
        let mut ids: Vec<String> = vectors
            .search("graph", &[1.0, 0.0, 0.0, 0.0], 32)
            .unwrap()
            .into_iter()
            .map(|hit| hit.id)
            .collect();
        ids.sort();
        ids
    }

    /// Async because the isolated queue's worker runs on the test's
    /// current-thread runtime: a blocking sleep here would starve it.
    async fn wait_for_graph_vectors(
        vectors: &crate::vectors::VectorStore,
        expected: &[String],
    ) -> Vec<String> {
        let mut expected = expected.to_vec();
        expected.sort();
        for _ in 0..300 {
            let ids = active_graph_vectors(vectors);
            if ids == expected {
                return ids;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        active_graph_vectors(vectors)
    }

    /// A campaign-shaped graph: `Idiom.statement` opts into embedding,
    /// `Idiom.status` is word-indexed only, `Note` has no opt-in at all.
    fn campaign_graph(
        project_id: &str,
        idioms: &[(&str, &str)],
        embeddings_enabled: bool,
    ) -> bbox_project_graph::GraphGeneration {
        // An `embed: true` annotation under a policy that forbids it is a
        // schema error, not a silent skip, so the policy-off generation
        // withdraws the annotation as a real author would.
        let statement_term = if embeddings_enabled {
            json!({"type": "string", "index": "text", "embed": true})
        } else {
            json!({"type": "string", "index": "text"})
        };
        let schema = serde_json::to_vec(&json!({
            "version": 1,
            "namespace": "campaign",
            "vertex_types": {
                "campaign:Idiom": {"properties": {
                    "statement": statement_term,
                    "status": {"type": "string", "index": "word"}
                }},
                "campaign:Note": {"properties": {"text": "string"}}
            },
            "edge_types": [],
            "index_policy": {"embeddings_enabled": embeddings_enabled}
        }))
        .unwrap();
        let mut vertices = Vec::new();
        for (id, statement) in idioms {
            vertices.push(
                serde_json::to_string(&json!({
                    "id": id,
                    "type": "campaign:Idiom",
                    "label": id,
                    "properties": {"statement": statement, "status": "active"}
                }))
                .unwrap(),
            );
        }
        vertices.push(
            serde_json::to_string(&json!({
                "id": "note/plain",
                "type": "campaign:Note",
                "label": "plain note",
                "properties": {"text": "never embedded"}
            }))
            .unwrap(),
        );
        let vertices = vertices.join("\n");
        let loaded = bbox_project_graph::load_graph_documents(
            project_id,
            GRAPH_ID,
            bbox_project_graph::GraphDocumentBytes {
                descriptor: None,
                schema: &schema,
                vertices: vertices.as_bytes(),
                edges: b"",
            },
            bbox_project_graph::GraphParseLimits::default(),
            std::path::PathBuf::new(),
        );
        assert!(loaded.report.valid, "{:?}", loaded.report.errors);
        loaded.generation.unwrap()
    }

    fn view(
        project_id: &ProjectId,
        generation: bbox_project_graph::GraphGeneration,
        stamp: &str,
    ) -> bbox_indexing::project_graph_view::PublishedProjectGraphView {
        bbox_indexing::project_graph_view::PublishedProjectGraphView {
            project_id: project_id.clone(),
            scope: PublishedScope::try_new("repo-campaigns", ".").unwrap(),
            accepted_generation: stamp.into(),
            graphs: std::collections::BTreeMap::from([(
                GRAPH_ID.to_string(),
                bbox_indexing::project_graph_view::ProjectGraphViewEntry::valid(
                    GRAPH_ID.to_string(),
                    bbox_indexing::project_graph_view::ProjectGraphGenerationIdentity {
                        accepted_generation: stamp.into(),
                        accepted_commit: "a".repeat(40),
                        source_generation: None,
                        workspace_id: None,
                        content_hash: generation.fingerprint.clone(),
                    },
                    generation,
                ),
            )]),
            evidence: bbox_project_graph::EvidenceBindingSet::default(),
        }
    }

    fn vertex_ref(project_id: &str, vertex_id: &str) -> String {
        format!("project_graph_vertex:{project_id}:{GRAPH_ID}:{vertex_id}")
    }

    #[tokio::test]
    async fn install_embeds_eligible_vertices_and_a_flip_tombstones_the_departed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let vectors = install_isolated_graph_queue(&root.join("vectors"));
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root.join("bro"))));
        let project = server
            .state
            .project_authority
            .bridge_registry()
            .unwrap()
            .write()
            .register_path(&root)
            .unwrap();
        let project_id = ProjectId::parse(project.project_id.clone()).unwrap();

        // Generation one: two eligible idioms plus a note that never embeds.
        let first = campaign_graph(
            project_id.as_str(),
            &[
                (
                    "idiom/delete-then-insert",
                    "replace rows by delete then insert",
                ),
                (
                    "idiom/push-durable",
                    "worker loss is lane loss; push durable work",
                ),
            ],
            true,
        );
        install_published_graph_view(
            &server.state,
            view(&project_id, first, "gen-one"),
            PublishedGraphViewInstaller::Test,
        );
        let expected = vec![
            vertex_ref(project_id.as_str(), "idiom/delete-then-insert"),
            vertex_ref(project_id.as_str(), "idiom/push-durable"),
        ];
        assert_eq!(
            wait_for_graph_vectors(&vectors, &expected).await,
            expected,
            "every embed-eligible vertex gets a vector; the note never does"
        );

        // The participation report says what embeds and how much of it did.
        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project.project_id.clone(),
                graph_id: GRAPH_ID.into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let wire = serde_json::to_value(&described).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        let described: serde_json::Value = serde_json::from_str(text).unwrap();
        let retrieval = &described["retrieval"];
        assert_eq!(retrieval["embeddings_enabled"], json!(true), "{text}");
        assert_eq!(retrieval["embed_eligible_vertex_count"], json!(2), "{text}");
        // The isolated queue carries no router, so activity is unknowable
        // here and must be reported as null rather than a phantom zero.
        assert!(retrieval["embedded_vertex_count"].is_null(), "{text}");

        // Generation two drops one idiom: its vector is tombstoned, the
        // survivor (unchanged projection) is not re-embedded or removed.
        let second = campaign_graph(
            project_id.as_str(),
            &[(
                "idiom/delete-then-insert",
                "replace rows by delete then insert",
            )],
            true,
        );
        install_published_graph_view(
            &server.state,
            view(&project_id, second, "gen-two"),
            PublishedGraphViewInstaller::Test,
        );
        let expected = vec![vertex_ref(project_id.as_str(), "idiom/delete-then-insert")];
        assert_eq!(wait_for_graph_vectors(&vectors, &expected).await, expected);

        // Policy off: the whole lane leaves the vector store.
        let third = campaign_graph(
            project_id.as_str(),
            &[(
                "idiom/delete-then-insert",
                "replace rows by delete then insert",
            )],
            false,
        );
        install_published_graph_view(
            &server.state,
            view(&project_id, third, "gen-three"),
            PublishedGraphViewInstaller::Test,
        );
        assert!(wait_for_graph_vectors(&vectors, &[]).await.is_empty());
        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphDescribeParams {
                project: project.project_id.clone(),
                graph_id: GRAPH_ID.into(),
                provisional: Some("published".into()),
                source: None,
                checkout_id: None,
                expected_content_hash: None,
                detail: None,
                cursor: None,
                body_limit: None,
                variant_limit: None,
                variant_offset: None,
                expected_view_stamp: None,
            }))
            .await;
        let wire = serde_json::to_value(&described).unwrap();
        let text = wire["content"][0]["text"].as_str().unwrap();
        let described: serde_json::Value = serde_json::from_str(text).unwrap();
        let retrieval = &described["retrieval"];
        assert_eq!(retrieval["embeddings_enabled"], json!(false), "{text}");
        assert_eq!(retrieval["embed_eligible_vertex_count"], json!(0), "{text}");
        assert_eq!(retrieval["embedded_vertex_count"], json!(0), "{text}");
    }
}

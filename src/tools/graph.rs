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
    /// the compact default.
    pub mode: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphListParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: Option<String>,
    /// Visibility policy: published, own, or all.
    pub visibility: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphExactParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: String,
    pub graph_id: String,
    /// Visibility policy: published, own, or all.
    pub visibility: Option<String>,
}

impl DescribeSchemaParams {
    fn include_agents_resolved(&self) -> bool {
        self.include_agents.unwrap_or_else(|| {
            self.mode
                .as_deref()
                .is_some_and(|m| matches!(m, "full" | "agents"))
        })
    }
}

#[tool_router(router = graph_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_inspect_entity",
        description = "Inspect a vertex: returns properties AND targeted edges in one call. Prefer targeted inspection over broad exploration: 1) Set edge_types to the specific edges you want (e.g. 'SUPERSEDES,DERIVED_FROM'). 2) Set direction to 'out' or 'in' when you know which way to traverse. 3) Use 'both' only for initial orientation on an unfamiliar entity. 4) Set per_type_limit=0 for property-only inspection. property_mode controls detail: 'summary' (names/titles only), 'smart' (full text <=300 chars, truncated for longer - default), 'full' (no truncation)."
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
        description = "List visible project graphs. Each entry reports two count families: vertex_count/edge_count are the REFLECTED graph (authored rows plus schema-as-data vertex/edge type definitions plus meta:INSTANCE_OF edges), while authored_vertex_count/authored_edge_count count only rows sourced from vertices.jsonl/edges.jsonl. Compare authored_* against your source files, not vertex_count/edge_count. Each entry's source names its authority plane: published, provisional, or connector (a read-only connector-managed source projection)."
    )]
    pub(crate) async fn bbox_project_graph_list(
        &self,
        Parameters(p): Parameters<ProjectGraphListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_list", move || {
            let graphs =
                server.project_graph_list_domain(p.project.as_deref(), p.visibility.as_deref())?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "visibility": p.visibility,
                "graphs": graphs,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_describe",
        description = "Describe one visible project graph. The summary carries both count families: vertex_count/edge_count are the REFLECTED graph (authored rows plus schema-as-data vertex/edge type definitions plus meta:INSTANCE_OF edges), while authored_vertex_count/authored_edge_count count only rows sourced from vertices.jsonl/edges.jsonl."
    )]
    pub(crate) async fn bbox_project_graph_describe(
        &self,
        Parameters(p): Parameters<ProjectGraphExactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_describe", move || {
            let graphs = server.project_graph_describe_domain(
                &p.project,
                &p.graph_id,
                p.visibility.as_deref(),
            )?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "graphs": graphs,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_validate",
        description = "Validate one visible project graph."
    )]
    pub(crate) async fn bbox_project_graph_validate(
        &self,
        Parameters(p): Parameters<ProjectGraphExactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_validate", move || {
            let graphs = server.project_graph_validate_domain(
                &p.project,
                &p.graph_id,
                p.visibility.as_deref(),
            )?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "graphs": graphs,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_describe_schema",
        description = "Catalog agentic-corpus entity types and edge families. Default is compact orientation for grounding: graph vocabulary, filterable fields, population counts, and traversal tips without the installed-agent catalog. Pass include_agents=true or mode=\"full\" only when you need installed-agent discovery."
    )]
    pub(crate) fn bbox_describe_schema(
        &self,
        Parameters(p): Parameters<DescribeSchemaParams>,
    ) -> CallToolResult {
        Self::run("bbox_describe_schema", || {
            let read_view = self.state.complete_code_read_view()?;
            let include_agents = p.include_agents_resolved();
            let agents = include_agents
                .then(|| self.build_agent_schema_entries())
                .unwrap_or_default();
            mcp_tools::describe_schema::describe_schema_with_options(
                &self.describe_schema_counts_from_view(&read_view),
                &agents,
                DescribeSchemaOptions { include_agents },
            )
        })
    }

    #[tool(
        name = "bbox_find_paths",
        description = "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent. A target is required: a call with neither to nor to_type is refused with error.bad_input rather than answered as an empty result. Over project graphs under own or all visibility, to_type='project_graph_vertex' also matches provisional overlay vertices, so the logical type is enough; pass to_type='provisional_project_graph_vertex' only to target the overlay form exactly."
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
        description = "Package selected entity refs and cached path IDs into a structured evidence bundle. Use after bbox_find_paths to close the loop before answering; stale path IDs degrade explicitly under degraded.stale_path_ids instead of failing the whole response. Set property_mode=summary for compact provenance bundles over broad/long refs; default is full for compatibility."
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
            knowledge_view.enrich_json_response(output)
        })
        .await
    }

    #[tool(
        name = "bbox_ref_size",
        description = "Measure the byte payload size of entity refs. file refs resolve through a validated current checkout attachment selected by exact project_dir, authoritative session checkout, or an unambiguous registered project; project_file and project_file_v2 refs resolve to full indexed chunk content without checkout access; other refs resolve through entity providers and measure serialized provider-properties JSON. Accepts up to 500 refs; successful refs are canonicalized and unresolved/omitted refs are reported under degraded."
    )]
    pub(crate) async fn bbox_ref_size(
        &self,
        Parameters(p): Parameters<RefSizeParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_ref_size", move || {
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
            Ok(output)
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
            if edge_index_rebuilt {
                crate::server::rebuild_edge_index_from_shared(&server.state, false)?;
            }
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "stats": stats,
                "edge_index_rebuilt": edge_index_rebuilt,
            }))?)
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
            },
        );
        project.project_id
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
                visibility: Some("published".into()),
            }))
            .await;
        let listed_text = extract_text(&listed);
        assert!(listed_text.contains("governance-record"), "{listed_text}");

        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphExactParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                visibility: Some("published".into()),
            }))
            .await;
        assert!(extract_text(&described).contains("governance-record-schema"));

        let validated = server
            .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                visibility: Some("published".into()),
            }))
            .await;
        assert!(extract_text(&validated).contains("\"valid\": true"));

        let vertex_ref =
            format!("project_graph_vertex:{project_id}:governance-record:record/case@1");
        let inspected = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
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
                },
            );

        let own = server
            .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                project: project_id.clone(),
                graph_id: "governance-record".into(),
                visibility: Some("own".into()),
            }))
            .await;
        let own_text = extract_text(&own);
        assert!(own_text.contains("\"valid\": false"), "{own_text}");
        assert!(own_text.contains("edge.missing_vertex"), "{own_text}");
        assert!(
            own_text.contains("\"source\": \"provisional\""),
            "{own_text}"
        );

        let published = server
            .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                project: project_id,
                graph_id: "governance-record".into(),
                visibility: Some("published".into()),
            }))
            .await;
        assert!(extract_text(&published).contains("\"valid\": true"));
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
        assert_ne!(resolved.is_error, Some(true), "{}", extract_text(&resolved));
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
        assert_ne!(compared.is_error, Some(true), "{}", extract_text(&compared));
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
        assert_ne!(resolved.is_error, Some(true), "{}", extract_text(&resolved));
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
        assert_eq!(body["agents"].as_array().unwrap().len(), 0);
        assert!(
            body["text"]
                .as_str()
                .unwrap()
                .contains("Omitted from compact orientation")
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
        }));
        assert_ne!(result.is_error, Some(true));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
        assert_eq!(body["agents_omitted"].as_bool(), Some(false));
        let agents = body["agents"].as_array().expect("agents array");
        assert_eq!(agents.len(), 2);
        let schema_tester = agents
            .iter()
            .find(|a| a["name"] == "schema-tester")
            .unwrap();
        assert_eq!(schema_tester["version"].as_str(), Some("1"));
        assert_eq!(schema_tester["cost_class"].as_str(), Some("normal"));
        assert_eq!(schema_tester["when_to_use"].as_array().unwrap().len(), 1);
        assert_eq!(schema_tester["anti_patterns"].as_array().unwrap().len(), 1);
        assert!(schema_tester["dispatch_adapter"].is_null());

        let badgey = agents.iter().find(|a| a["name"] == "badgey-agent").unwrap();
        assert_eq!(badgey["dispatch_adapter"].as_str(), Some("badgey"));
        assert_eq!(
            badgey["when_to_use"]
                .as_array()
                .expect("when_to_use always present"),
            &Vec::<serde_json::Value>::new(),
            "badgey-agent has empty when_to_use but field must be present"
        );
        assert_eq!(
            badgey["anti_patterns"]
                .as_array()
                .expect("anti_patterns always present"),
            &Vec::<serde_json::Value>::new(),
            "badgey-agent has empty anti_patterns but field must be present"
        );

        let by_adapter = body["agents_by_dispatch_adapter"]
            .as_object()
            .expect("agents_by_dispatch_adapter object");
        assert_eq!(by_adapter["direct"].as_array().unwrap().len(), 1);
        assert_eq!(by_adapter["badgey"].as_array().unwrap().len(), 1);
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

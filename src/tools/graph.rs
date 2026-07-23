use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use bbox_corpus_core::identity::PublishedScope;
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

const REF_SIZE_CAP: usize = 500;

#[derive(Debug, Clone)]
struct CheckoutFileSelection {
    project: ProjectRecord,
    attachment: CheckoutAttachmentSelector,
    expected_scope: Option<PublishedScope>,
    source_lane: CheckoutAccessSourceLane,
    relative_path: PathBuf,
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

fn acquire_selected_operation(
    broker: &CheckoutAccessBroker,
    project_id: &str,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<ValidatedCheckoutLease> {
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
    if scope.bbox_root_relpath == "." {
        return Ok(checkout_root.to_path_buf());
    }
    let relative = Path::new(&scope.bbox_root_relpath);
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

fn selected_file_selection(
    project: ProjectRecord,
    relative_path: PathBuf,
) -> CheckoutFileSelection {
    CheckoutFileSelection {
        expected_scope: None,
        project,
        attachment: CheckoutAttachmentSelector::Selected,
        source_lane: CheckoutAccessSourceLane::LegacyProjectRecord,
        relative_path,
    }
}

fn checkout_file_selection(
    project: ProjectRecord,
    scope: PublishedScope,
    checkout_id: String,
    relative_path: PathBuf,
) -> CheckoutFileSelection {
    CheckoutFileSelection {
        project,
        attachment: CheckoutAttachmentSelector::CheckoutId(checkout_id),
        expected_scope: Some(scope),
        source_lane: CheckoutAccessSourceLane::LegacyCheckoutRegistry,
        relative_path,
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
            selected_file_selection(project.clone(), relative.to_path_buf()),
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
                    project.clone(),
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
        selection.relative_path = relative.to_path_buf();
        return Ok(selection);
    }

    if let Some(session) = session_checkout {
        let project = unique_project(projects, &session.project_id)?;
        return Ok(checkout_file_selection(
            project,
            session.published_scope.clone(),
            session.checkout_id.clone(),
            relative.to_path_buf(),
        ));
    }

    match projects {
        [project] => Ok(selected_file_selection(
            project.clone(),
            relative.to_path_buf(),
        )),
        [] => bail!("error.project_not_registered: no registered project can resolve the file"),
        _ => bail!(
            "error.project_ambiguous: relative file requires project_dir or authoritative session checkout"
        ),
    }
}

fn acquire_file_selection(
    broker: &CheckoutAccessBroker,
    selection: CheckoutFileSelection,
    kind: CheckoutAccessKind,
    intent: CheckoutAccessIntent,
) -> Result<AcquiredCheckoutFile> {
    let project_id = selection.project.project_id;
    let lease = if selection.attachment == CheckoutAttachmentSelector::Selected {
        acquire_selected_operation(broker, &project_id, kind, intent)?
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
    let (_, content) = lease
        .read_relative_file(&selection.relative_path)
        .map_err(checkout_access_error)?;
    Ok(AcquiredCheckoutFile {
        lease,
        relative_path: selection.relative_path.to_string_lossy().into_owned(),
        content,
    })
}

fn file_selection(
    broker: &CheckoutAccessBroker,
    input: &str,
    project_dir: Option<&str>,
    session_checkout: Option<&ResolvedCheckoutScope>,
    projects: &[ProjectRecord],
    rows: &[CheckoutRow],
) -> Result<CheckoutFileSelection> {
    let path = Path::new(input);
    if path.is_absolute() {
        absolute_file_selection(broker, path, projects, rows)
    } else {
        relative_file_selection(broker, path, project_dir, session_checkout, projects, rows)
    }
}

fn acquire_project_file(
    broker: &CheckoutAccessBroker,
    project_id: &str,
    indexed_path_hint: &Path,
    projects: &[ProjectRecord],
) -> Result<AcquiredCheckoutFile> {
    let project = unique_project(projects, project_id)?;
    let lease = acquire_selected_operation(
        broker,
        &project.project_id,
        CheckoutAccessKind::Blame,
        CheckoutAccessIntent::Read,
    )?;
    let relative = if indexed_path_hint.is_absolute() {
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

fn requested_provenance_projects(
    params: &ProvenanceParams,
    projects: &[ProjectRecord],
) -> Result<Vec<ProjectRecord>> {
    if let Some(project_id) = params.project_id.as_deref() {
        return Ok(vec![unique_project(projects, project_id)?]);
    }
    let mut seen = HashSet::new();
    for project in projects {
        if !seen.insert(project.project_id.as_str()) {
            bail!("error.project_ambiguous: registered project ids are not unique");
        }
    }
    Ok(projects.to_vec())
}

fn acquire_provenance_projects(
    broker: &CheckoutAccessBroker,
    params: &ProvenanceParams,
    projects: &[ProjectRecord],
    intent: CheckoutAccessIntent,
) -> Result<(
    Vec<ValidatedCheckoutLease>,
    Vec<mcp_tools::provenance::ProvenanceProject>,
)> {
    let requested = requested_provenance_projects(params, projects)?;
    let mut leases = Vec::with_capacity(requested.len());
    let mut inputs = Vec::with_capacity(requested.len());
    for project in requested {
        let lease = acquire_selected_operation(
            broker,
            &project.project_id,
            CheckoutAccessKind::ProvenanceNoteIo,
            intent,
        )?;
        inputs.push(mcp_tools::provenance::ProvenanceProject {
            project_id: project.project_id,
            project_root: lease.project_root().to_path_buf(),
        });
        leases.push(lease);
    }
    Ok((leases, inputs))
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
            let read_view = server.state.code_read_view.read().clone();
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
                .with_searcher(&read_view.searcher);
            let output =
                mcp_tools::inspect::inspect_entity(&p, &provider_ctx, &entity_ref, edge_index)?;
            knowledge_view.enrich_json_response(output)
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
            let include_agents = p.include_agents_resolved();
            let agents = include_agents
                .then(|| self.build_agent_schema_entries())
                .unwrap_or_default();
            mcp_tools::describe_schema::describe_schema_with_options(
                &self.describe_schema_counts(),
                &agents,
                DescribeSchemaOptions { include_agents },
            )
        })
    }

    #[tool(
        name = "bbox_find_paths",
        description = "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent."
    )]
    pub(crate) async fn bbox_find_paths(
        &self,
        Parameters(p): Parameters<FindPathsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_find_paths", move || {
            let read_view = server.state.code_read_view.read().clone();
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher);
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
            let knowledge_view = server.session_knowledge_view(None, p.provisional.as_deref())?;
            let read_view = server.state.code_read_view.read().clone();
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_knowledge_view(&knowledge_view.knowledge)
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher);
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
            let projects = server.state.projects.read().list();
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
                    &broker,
                    &path,
                    p.project_dir.as_deref(),
                    session_checkout.as_deref(),
                    &projects,
                    &checkout_rows,
                )
                .and_then(|selection| {
                    acquire_file_selection(
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
            let read_view = server.state.code_read_view.read().clone();
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
            let edges_dir = edge_index::edges_dir_from_bro_store(&server.state.store_dir);
            let apply = p.apply.unwrap_or(false);
            let stats = edge_index::compact_legacy_sidecar(&edges_dir, &p.project_id, apply)?;
            let edge_index_rebuilt = apply && p.rebuild.unwrap_or(false);
            if edge_index_rebuilt {
                crate::server::rebuild_edge_index_from_shared(&server.state, false);
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
            let read_view = server.state.code_read_view.read().clone();
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx = server
                .provider_context()
                .with_edge_index(edge_index)
                .with_searcher(&read_view.searcher);
            let projects = server.state.projects.read().list();
            let target = match mcp_tools::blame::target_identity(&p, &provider_ctx) {
                Ok(target) => target,
                Err(error) => return Ok(mcp_tools::blame::bad_input(error.to_string())),
            };
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let acquired = match target {
                mcp_tools::blame::BlameTargetIdentity::ProjectFile {
                    project_id,
                    indexed_path_hint,
                    line,
                    byte_offset,
                } => {
                    let acquired =
                        acquire_project_file(&broker, &project_id, &indexed_path_hint, &projects)?;
                    (acquired, line, Some(byte_offset))
                }
                mcp_tools::blame::BlameTargetIdentity::File { input_path, line } => {
                    let checkout_rows = server.state.checkout_registry.read().rows().to_vec();
                    let selection = file_selection(
                        &broker,
                        &input_path,
                        None,
                        server.authoritative_session_checkout().as_deref(),
                        &projects,
                        &checkout_rows,
                    )?;
                    let acquired = acquire_file_selection(
                        &broker,
                        selection,
                        CheckoutAccessKind::Blame,
                        CheckoutAccessIntent::Read,
                    )?;
                    (acquired, Some(line), None)
                }
            };
            let (mut acquired, line, byte_offset) = acquired;
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
                    content: std::mem::take(&mut acquired.content),
                    display_path: acquired.relative_path.clone(),
                    line,
                    byte_offset,
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
            let projects = server.state.projects.read().list();
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let (leases, inputs) =
                acquire_provenance_projects(&broker, &p, &projects, CheckoutAccessIntent::Write)?;
            let read_view = server.state.code_read_view.read().clone();
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
            let checkout = server.authoritative_session_checkout().ok_or_else(|| {
                anyhow::anyhow!(
                    "error.no_authoritative_checkout: initialize MCP with an admitted project context"
                )
            })?;
            if checkout.project_id.trim().is_empty()
                || checkout.published_scope.repo_id.trim().is_empty()
                || checkout.published_scope.bbox_root_relpath.trim().is_empty()
            {
                anyhow::bail!(
                    "error.invalid_checkout_scope: authoritative checkout has no durable project scope"
                );
            }
            let projects = server.state.projects.read().list();
            if !projects
                .iter()
                .any(|project| project.project_id == checkout.project_id)
            {
                anyhow::bail!(
                    "error.project_not_registered: authoritative checkout project is absent from the registry"
                );
            }
            let notes_ref = git::notes_ref("provenance");
            let page = mcp_tools::provenance_plan::export_plan_page(
                &p,
                checkout.published_scope.clone(),
                &checkout.project_id,
                &notes_ref,
                server
                    .state
                    .code_read_view
                    .read()
                    .edge_index
                    .as_ref(),
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
            let projects = server.state.projects.read().list();
            let broker = crate::server::checkout_access::checkout_access_broker(&server.state);
            let (leases, inputs) =
                acquire_provenance_projects(&broker, &p, &projects, CheckoutAccessIntent::Read)?;
            let edges_dir = edge_index::edges_dir_from_bro_store(&server.state.store_dir);
            let resolve_legacy_target =
                |project_id: &str,
                 root: &Path,
                 absolute_path: &Path,
                 byte_range: Option<(u64, u64)>| {
                    let project = unique_project(&projects, project_id)?;
                    bbox_indexing::index::resolve_current_project_chunk_entity(
                        &project,
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
                server.rebuild_edge_index_from_stores();
                drop(publication);
                imported
            };
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "edges_imported": edges_imported,
                "notes_ref": git::notes_ref("provenance"),
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
        let scope = PublishedScope {
            repo_id: "repo-one".into(),
            bbox_root_relpath: ".".into(),
        };
        let (broker, requests, _) = recording_broker_with_scopes(
            HashMap::from([("project-one".into(), root)]),
            HashMap::from([("project-one".into(), scope.clone())]),
        );

        let lease = acquire_selected_operation(
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

        let error = acquire_file_selection(
            &broker,
            selected_file_selection(project, PathBuf::from("file.rs")),
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
            published_scope: PublishedScope {
                repo_id: "repo-one".into(),
                bbox_root_relpath: ".".into(),
            },
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

        let acquired = acquire_file_selection(
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
        let scope = PublishedScope {
            repo_id: "repo-one".into(),
            bbox_root_relpath: ".".into(),
        };
        let row = CheckoutRow {
            project_id: None,
            checkout_id: "checkout-one".into(),
            checkout_dir: checkout.to_string_lossy().into_owned(),
            repo_id: Some(scope.repo_id.clone()),
            bbox_root_relpath: Some(scope.bbox_root_relpath.clone()),
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
        let acquired = acquire_file_selection(
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

        let error = acquire_file_selection(
            &broker,
            selected_file_selection(project, PathBuf::from("../escape.rs")),
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

        let (leases, inputs) = acquire_provenance_projects(
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
            }])),
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
    async fn provenance_export_plan_requires_session_project_to_remain_registered() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = test_server(&tmp);
        server.set_session_checkout_for_test(
            "unregistered-project".into(),
            bbox_corpus_core::identity::PublishedScope {
                repo_id: "repo".into(),
                bbox_root_relpath: ".".into(),
            },
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

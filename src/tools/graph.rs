use crate::server::*;
use crate::*;
use rmcp::schemars;
use serde::{Deserialize, Serialize};

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

#[tool_router(router = graph_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_inspect_entity",
        description = "Inspect a vertex: returns properties AND targeted edges in one call. Prefer targeted inspection over broad exploration: 1) Set edge_types to the specific edges you want (e.g. 'SUPERSEDES,DERIVED_FROM'). 2) Set direction to 'out' or 'in' when you know which way to traverse. 3) Use 'both' only for initial orientation on an unfamiliar entity. 4) Set per_type_limit=0 for property-only inspection. property_mode controls detail: 'summary' (names/titles only), 'smart' (full text <=300 chars, truncated for longer - default), 'full' (no truncation)."
    )]
    pub(crate) fn bbox_inspect_entity(
        &self,
        Parameters(p): Parameters<InspectEntityParams>,
    ) -> CallToolResult {
        Self::run("bbox_inspect_entity", || {
            let entity_ref = match crate::entity_ref::EntityRef::parse(&p.entity_ref) {
                Ok(entity_ref) => entity_ref,
                Err(err) => {
                    return Ok(mcp_tools::inspect::bad_input(
                        &p.entity_ref,
                        err.to_string(),
                    ));
                }
            };
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::inspect::inspect_entity(
                &p,
                &provider_ctx,
                &entity_ref,
                &self.state.edge_index.read(),
            )
        })
    }

    #[tool(
        name = "bbox_describe_schema",
        description = "Catalog agentic-corpus entity types, edge families, and installed agents. Use before bbox_inspect_entity, bbox_find_paths, or evidence bundling when you need the graph vocabulary, filterable fields, population counts, or traversal tips. Also use for installed-agent discovery: the agents section lists name, version, description, when_to_use, anti_patterns, cost_class, and example invocation for every active agent, grouped by dispatch_adapter."
    )]
    pub(crate) fn bbox_describe_schema(&self) -> CallToolResult {
        Self::run("bbox_describe_schema", || {
            let agents = self.build_agent_schema_entries();
            mcp_tools::describe_schema::describe_schema(&self.describe_schema_counts(), &agents)
        })
    }

    #[tool(
        name = "bbox_find_paths",
        description = "Find direction-preserving graph paths from one EntityRef to another ref or entity type. Use after bbox_inspect_entity when a claim depends on a multi-hop chain; filter edge_types aggressively, keep max_depth small (default 3, max 5), and reuse returned path IDs with bbox_bundle_evidence. edge_types accepts a comma-separated string (e.g. 'CALLS,CALLED_BY') OR a JSON array of strings. Both shapes are equivalent."
    )]
    pub(crate) fn bbox_find_paths(
        &self,
        Parameters(p): Parameters<FindPathsParams>,
    ) -> CallToolResult {
        Self::run("bbox_find_paths", || {
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::find_paths::find_paths(
                &p,
                &provider_ctx,
                &self.state.edge_index.read(),
                &mut self.state.path_cache.write(),
            )
        })
    }

    #[tool(
        name = "bbox_bundle_evidence",
        description = "Package selected entity refs and cached path IDs into a structured evidence bundle. Use after bbox_find_paths to close the loop before answering; stale path IDs degrade explicitly under degraded.stale_path_ids instead of failing the whole response."
    )]
    pub(crate) fn bbox_bundle_evidence(
        &self,
        Parameters(p): Parameters<BundleEvidenceParams>,
    ) -> CallToolResult {
        Self::run("bbox_bundle_evidence", || {
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::bundle_evidence::bundle_evidence(
                &p,
                &provider_ctx,
                &self.state.edge_index.read(),
                &mut self.state.path_cache.write(),
            )
        })
    }

    #[tool(
        name = "bbox_ref_size",
        description = "Measure the byte payload size of entity refs. Project-file and project_file_v2 refs resolve to full indexed chunk content; other refs resolve through entity providers and measure serialized provider-properties JSON. Accepts up to 500 refs; successful refs are canonicalized and unresolved/omitted refs are reported under degraded."
    )]
    pub(crate) fn bbox_ref_size(&self, Parameters(p): Parameters<RefSizeParams>) -> CallToolResult {
        Self::run("bbox_ref_size", || {
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::ref_size::ref_size(&p, &provider_ctx)
        })
    }

    #[tool(
        name = "bbox_edge_compact",
        description = "Dry-run or apply legacy edge sidecar compaction for one project. Removes append-only derived edges from edges/<project_id>.jsonl while retaining explicit/provenance/malformed lines; apply defaults false and writes a backup before replacement. With apply=true, rebuild=true forces a sidecar-only in-memory EdgeIndex rebuild even when compaction is already complete."
    )]
    pub(crate) fn bbox_edge_compact(
        &self,
        Parameters(p): Parameters<EdgeCompactParams>,
    ) -> CallToolResult {
        Self::run("bbox_edge_compact", || {
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
            let apply = p.apply.unwrap_or(false);
            let stats = edge_index::compact_legacy_sidecar(&edges_dir, &p.project_id, apply)?;
            let edge_index_rebuilt = apply && p.rebuild.unwrap_or(false);
            if edge_index_rebuilt {
                crate::server::rebuild_edge_index_from_shared(&self.state, false);
            }
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "stats": stats,
                "edge_index_rebuilt": edge_index_rebuilt,
            }))?)
        })
    }

    #[tool(
        name = "bbox_blame",
        description = "Walk back from a code line to the conversation that produced it. Two modes: 1. Anchor-matching: the line's git blame commit matches a bbox-tracked tool-call anchor, returning the full session/brofile/arc/trigger chain. 2. Git-only fallback: no bbox anchor matches, returning git blame author info only, marked as non-bbox. Use this when you want to understand WHY a line exists, not just WHO wrote it."
    )]
    pub(crate) fn bbox_blame(&self, Parameters(p): Parameters<BlameParams>) -> CallToolResult {
        Self::run("bbox_blame", || {
            let provider_ctx = ProviderContext::new(&self.state);
            let projects = self.state.projects.read().list();
            mcp_tools::blame::blame(&p, &provider_ctx, &self.state.edge_index.read(), &projects)
        })
    }

    #[tool(
        name = "bbox_provenance_export",
        description = "Write bbox provenance git notes for commits with tracked tool-call anchors."
    )]
    pub(crate) fn bbox_provenance_export(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        Self::run("bbox_provenance_export", || {
            let projects = self.state.projects.read().list();
            mcp_tools::provenance::export_provenance(&p, &self.state.edge_index.read(), &projects)
        })
    }

    #[tool(
        name = "bbox_provenance_import",
        description = "Read bbox provenance git notes and replay them into the local EdgeIndex sidecar."
    )]
    pub(crate) fn bbox_provenance_import(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        Self::run("bbox_provenance_import", || {
            let projects = self.state.projects.read().list();
            let edges_dir = edge_index::edges_dir_from_bro_store(&self.state.store_dir);
            let edges_imported =
                mcp_tools::provenance::import_provenance_to_edges_dir(&p, &projects, &edges_dir)?;
            self.rebuild_edge_index_from_stores();
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "edges_imported": edges_imported,
                "notes_ref": crate::git::notes_ref("provenance"),
            }))?)
        })
    }
}

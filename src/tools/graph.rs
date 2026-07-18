use crate::mcp_tools;
use crate::mcp_tools::blame::BlameParams;
use crate::mcp_tools::bundle_evidence::BundleEvidenceParams;
use crate::mcp_tools::describe_schema::DescribeSchemaOptions;
use crate::mcp_tools::find_paths::FindPathsParams;
use crate::mcp_tools::inspect::InspectEntityParams;
use crate::mcp_tools::provenance::ProvenanceParams;
use crate::mcp_tools::ref_size::RefSizeParams;
use crate::providers::ProviderContext;
use crate::server::BlackboxServer;
use crate::{edge_index, entity_ref, git};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::path::PathBuf;

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
    /// Registered project id, alias, base path, or worktree path. Omit to
    /// list committed graphs for every registered project.
    pub project: Option<String>,
    /// Include `.bbox/local/graphs` scratch graphs. Defaults false.
    #[serde(default)]
    pub include_local: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct ProjectGraphExactParams {
    /// Registered project id, alias, base path, or worktree path.
    pub project: String,
    pub graph_id: String,
    /// Include `.bbox/local/graphs` scratch graphs. Defaults false.
    #[serde(default)]
    pub include_local: Option<bool>,
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
        Self::run_blocking("bbox_inspect_entity", move || {
            let entity_ref = match entity_ref::EntityRef::parse(&p.entity_ref) {
                Ok(entity_ref) => entity_ref,
                Err(err) => {
                    return Ok(mcp_tools::inspect::bad_input(
                        &p.entity_ref,
                        err.to_string(),
                    ));
                }
            };
            let include_local = p.include_local_graphs.unwrap_or(false);
            if let Some(error) = crate::project_graph_runtime::refresh_ref_error(
                &server.state,
                &entity_ref,
                include_local,
            ) {
                return Ok(error);
            }
            let edge_index = server.state.edge_index.read();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(&edge_index)
                    .with_local_project_graphs(include_local);
            mcp_tools::inspect::inspect_entity(&p, &provider_ctx, &entity_ref, &edge_index)
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
        name = "bbox_project_graph_list",
        description = "List project-owned reflective graphs and their validation status. Committed .bbox/graphs entries are included by default; .bbox/local/graphs scratch entries require include_local=true."
    )]
    pub(crate) async fn bbox_project_graph_list(
        &self,
        Parameters(p): Parameters<ProjectGraphListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_list", move || {
            let include_local = p.include_local.unwrap_or(false);
            let projects = match graph_projects(&server, p.project.as_deref()) {
                Ok(projects) => projects,
                Err(error) => return Ok(error),
            };
            crate::project_graph_runtime::list_graphs(&server.state, projects, include_local)
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_validate",
        description = "Structurally validate one project graph and atomically publish its complete generation when valid. Reports stable error codes with source file and JSONL line numbers. Scratch graphs require include_local=true."
    )]
    pub(crate) async fn bbox_project_graph_validate(
        &self,
        Parameters(p): Parameters<ProjectGraphExactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_validate", move || {
            let include_local = p.include_local.unwrap_or(false);
            let (scope_id, root) = match graph_project(&server, &p.project) {
                Ok(project) => project,
                Err(error) => return Ok(error),
            };
            crate::project_graph_runtime::validate_graph(
                &server.state,
                &scope_id,
                &root,
                &p.graph_id,
                include_local,
            )
        })
        .await
    }

    #[tool(
        name = "bbox_project_graph_describe",
        description = "Describe one accepted reflective graph generation, including its descriptor, project schema document, fixed meta-schema floor, counts, and source location. Scratch graphs require include_local=true."
    )]
    pub(crate) async fn bbox_project_graph_describe(
        &self,
        Parameters(p): Parameters<ProjectGraphExactParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_project_graph_describe", move || {
            let include_local = p.include_local.unwrap_or(false);
            let (scope_id, root) = match graph_project(&server, &p.project) {
                Ok(project) => project,
                Err(error) => return Ok(error),
            };
            crate::project_graph_runtime::describe_graph(
                &server.state,
                &scope_id,
                &root,
                &p.graph_id,
                include_local,
            )
        })
        .await
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
            let include_local = p.include_local_graphs.unwrap_or(false);
            for raw in std::iter::once(Some(p.from.as_str()))
                .chain(std::iter::once(p.to.as_deref()))
                .flatten()
            {
                if let Ok(entity) = entity_ref::EntityRef::parse(raw)
                    && let Some(error) = crate::project_graph_runtime::refresh_ref_error(
                        &server.state,
                        &entity,
                        include_local,
                    )
                {
                    return Ok(error);
                }
            }
            let edge_index = server.state.edge_index.read();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(&edge_index)
                    .with_local_project_graphs(include_local);
            mcp_tools::find_paths::find_paths(
                &p,
                &provider_ctx,
                &edge_index,
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
        Self::run_blocking("bbox_bundle_evidence", move || {
            let include_local = p.include_local_graphs.unwrap_or(false);
            for raw in &p.entity_refs {
                if let Ok(entity) = entity_ref::EntityRef::parse(raw)
                    && let Some(error) = crate::project_graph_runtime::refresh_ref_error(
                        &server.state,
                        &entity,
                        include_local,
                    )
                {
                    return Ok(error);
                }
            }
            let edge_index = server.state.edge_index.read();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(&edge_index)
                    .with_local_project_graphs(include_local);
            mcp_tools::bundle_evidence::bundle_evidence(
                &p,
                &provider_ctx,
                &edge_index,
                &mut server.state.path_cache.write(),
            )
        })
        .await
    }

    #[tool(
        name = "bbox_ref_size",
        description = "Measure the byte payload size of entity refs. file refs resolve against optional project_dir first, then registered project file content; project_file and project_file_v2 refs resolve to full indexed chunk content; other refs resolve through entity providers and measure serialized provider-properties JSON. Accepts up to 500 refs; successful refs are canonicalized and unresolved/omitted refs are reported under degraded."
    )]
    pub(crate) async fn bbox_ref_size(
        &self,
        Parameters(p): Parameters<RefSizeParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_ref_size", move || {
            for raw in &p.refs {
                if let Ok(entity) = entity_ref::EntityRef::parse(raw)
                    && let Some(error) = crate::project_graph_runtime::refresh_ref_error(
                        &server.state,
                        &entity,
                        false,
                    )
                {
                    return Ok(error);
                }
            }
            let edge_index = server.state.edge_index.read();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(&edge_index);
            mcp_tools::ref_size::ref_size(&p, &provider_ctx)
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
            let edge_index = server.state.edge_index.read();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(&edge_index);
            let projects = server.state.projects.read().list();
            mcp_tools::blame::blame(&p, &provider_ctx, &edge_index, &projects)
        })
        .await
    }

    #[tool(
        name = "bbox_provenance_export",
        description = "Write bbox provenance git notes for commits with tracked tool-call anchors."
    )]
    pub(crate) async fn bbox_provenance_export(
        &self,
        Parameters(p): Parameters<ProvenanceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_provenance_export", move || {
            let projects = server.state.projects.read().list();
            mcp_tools::provenance::export_provenance(&p, &server.state.edge_index.read(), &projects)
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
            let edges_dir = edge_index::edges_dir_from_bro_store(&server.state.store_dir);
            let edges_imported =
                mcp_tools::provenance::import_provenance_to_edges_dir(&p, &projects, &edges_dir)?;
            server.rebuild_edge_index_from_stores();
            Ok(serde_json::to_string_pretty(&json!({
                "status": "ok",
                "edges_imported": edges_imported,
                "notes_ref": git::notes_ref("provenance"),
            }))?)
        })
        .await
    }
}

fn graph_project(server: &BlackboxServer, raw: &str) -> Result<(String, PathBuf), String> {
    let projects = server.state.projects.read().list();
    let Some(context) = crate::projects::resolve_project_context(
        raw,
        &projects,
        crate::projects::ResolveIntent::Read,
    ) else {
        return Err(project_graph_bad_input(raw));
    };
    let root = context
        .checkout
        .map(|checkout| PathBuf::from(checkout.checkout_dir))
        .unwrap_or_else(|| PathBuf::from(context.host_root));
    Ok((context.project_id, root))
}

fn graph_projects(
    server: &BlackboxServer,
    raw: Option<&str>,
) -> Result<Vec<(String, PathBuf)>, String> {
    match raw {
        Some(raw) => graph_project(server, raw).map(|project| vec![project]),
        None => Ok(server
            .state
            .projects
            .read()
            .list()
            .into_iter()
            .map(|project| (project.project_id, PathBuf::from(project.canonical_path)))
            .collect()),
    }
}

fn project_graph_bad_input(raw: &str) -> String {
    json!({
        "status": "error.bad_input",
        "error": {
            "code": "error.bad_input",
            "field": "project",
            "message": format!("project `{raw}` is not registered"),
            "suggested_fix": "Pass a registered project id, alias, base path, or worktree path."
        }
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts;
    use crate::server::state::SharedState;
    use std::fs;
    use std::path::Path;
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn write_project_graph_fixture(
        root: &Path,
        graph_id: &str,
        local: bool,
        schema: serde_json::Value,
        vertices: Vec<serde_json::Value>,
        edges: Vec<serde_json::Value>,
    ) {
        let relative = if local {
            ".bbox/local/graphs"
        } else {
            ".bbox/graphs"
        };
        let dir = root.join(relative).join(graph_id);
        fs::create_dir_all(&dir).unwrap();
        let retention = if local {
            "local_scratch"
        } else {
            "project_owned"
        };
        fs::write(
            dir.join("graph.json"),
            serde_json::to_vec_pretty(&json!({
                "descriptor_version": 1,
                "scope": "project",
                "graph_id": graph_id,
                "authority": "project",
                "schema_id": format!("{graph_id}-schema"),
                "schema_version": schema["version"],
                "retention_policy": retention,
                "generation": 1,
            }))
            .unwrap(),
        )
        .unwrap();
        fs::write(
            dir.join("schema.json"),
            serde_json::to_vec_pretty(&schema).unwrap(),
        )
        .unwrap();
        let jsonl = |rows: Vec<serde_json::Value>| {
            let mut text = rows
                .into_iter()
                .map(|row| serde_json::to_string(&row).unwrap())
                .collect::<Vec<_>>()
                .join("\n");
            if !text.is_empty() {
                text.push('\n');
            }
            text
        };
        fs::write(dir.join("vertices.jsonl"), jsonl(vertices)).unwrap();
        fs::write(dir.join("edges.jsonl"), jsonl(edges)).unwrap();
    }

    #[tokio::test]
    async fn two_unrelated_schemas_validate_describe_inspect_traverse_and_bundle() {
        let store = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().canonicalize().unwrap();
        write_project_graph_fixture(
            &project_root,
            "repo",
            false,
            json!({
                "version": 1,
                "namespace": "repo",
                "vertex_types": {
                    "repo:Module": {
                        "required": ["path", "source"],
                        "properties": {
                            "path": "string",
                            "source": {"file": "string", "tags": ["string"]}
                        }
                    },
                    "repo:Invariant": {
                        "required": ["claim"],
                        "properties": {"claim": "string"}
                    }
                },
                "edge_types": [{
                    "type": "repo:CONSTRAINED_BY",
                    "from_type": "repo:Module",
                    "to_type": "repo:Invariant",
                    "required": ["confidence"],
                    "properties": {"confidence": "number"}
                }]
            }),
            vec![
                json!({
                    "id": "src/tools/graph.rs",
                    "type": "repo:Module",
                    "label": "graph tools",
                    "properties": {
                        "path": "src/tools/graph.rs",
                        "source": {"file": "PROJECT.md", "tags": ["graph", "tools"]}
                    }
                }),
                json!({
                    "id": "canonical-refs",
                    "type": "repo:Invariant",
                    "label": "canonical refs",
                    "properties": {"claim": "entity refs round trip"}
                }),
            ],
            vec![json!({
                "from": "src/tools/graph.rs",
                "type": "repo:CONSTRAINED_BY",
                "to": "canonical-refs",
                "properties": {"confidence": 1}
            })],
        );
        write_project_graph_fixture(
            &project_root,
            "deployments",
            false,
            json!({
                "version": 1,
                "namespace": "ops",
                "vertex_types": {
                    "ops:Service": {
                        "required": ["healthy", "owners"],
                        "properties": {"healthy": "boolean", "owners": ["string"]}
                    },
                    "ops:Region": {
                        "required": ["capacity"],
                        "properties": {"capacity": "number"}
                    }
                },
                "edge_types": [{
                    "type": "ops:DEPLOYED_IN",
                    "from_type": "ops:Service",
                    "to_type": "ops:Region",
                    "properties": {"rollout": {"wave": "number", "approved": "boolean"}}
                }]
            }),
            vec![
                json!({
                    "id": "api",
                    "type": "ops:Service",
                    "label": "public api",
                    "properties": {"healthy": true, "owners": ["platform"]}
                }),
                json!({
                    "id": "north",
                    "type": "ops:Region",
                    "label": "north region",
                    "properties": {"capacity": 3}
                }),
            ],
            vec![json!({
                "from": "api",
                "type": "ops:DEPLOYED_IN",
                "to": "north",
                "properties": {"rollout": {"wave": 2, "approved": true}}
            })],
        );

        let server = test_server(&store);
        let record = server
            .state
            .projects
            .write()
            .register_path(&project_root)
            .unwrap();
        let project_selector = project_root.to_string_lossy().into_owned();
        for graph_id in ["repo", "deployments"] {
            let result = server
                .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                    project: project_selector.clone(),
                    graph_id: graph_id.into(),
                    include_local: None,
                }))
                .await;
            let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
            assert_eq!(body["validation"]["valid"], true, "{body}");
            assert_eq!(body["accepted"], true, "{body}");
        }

        let described = server
            .bbox_project_graph_describe(Parameters(ProjectGraphExactParams {
                project: project_selector,
                graph_id: "repo".into(),
                include_local: None,
            }))
            .await;
        let described: serde_json::Value = serde_json::from_str(&extract_text(&described)).unwrap();
        assert_eq!(described["status"], "ok");
        assert_eq!(
            described["meta_schema"]["vertex_types"][0],
            "meta:VertexType"
        );
        assert_eq!(described["schema"]["namespace"], "repo");

        let module_ref = format!(
            "project_graph_vertex:{}:repo:src/tools/graph.rs",
            record.project_id
        );
        let invariant_ref = format!(
            "project_graph_vertex:{}:repo:canonical-refs",
            record.project_id
        );
        let inspected = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: module_ref.clone(),
                edge_types: None,
                direction: Some("out".into()),
                per_type_limit: Some(10),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let inspected: serde_json::Value = serde_json::from_str(&extract_text(&inspected)).unwrap();
        assert_eq!(inspected["status"], "ok", "{inspected}");
        assert_eq!(inspected["properties"]["type"], "repo:Module");
        assert!(
            inspected["edges"]["out"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["kind"] == "repo:CONSTRAINED_BY" && edge["properties"]["confidence"] == "1"
                })
        );

        let paths = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: module_ref.clone(),
                to: Some(invariant_ref.clone()),
                to_type: None,
                edge_types: Some(crate::mcp_tools::find_paths::EdgeTypesParam::One(
                    "repo:CONSTRAINED_BY".into(),
                )),
                max_depth: Some(2),
                limit: Some(5),
                include_local_graphs: None,
            }))
            .await;
        let paths: serde_json::Value = serde_json::from_str(&extract_text(&paths)).unwrap();
        assert_eq!(paths["paths"].as_array().unwrap().len(), 1, "{paths}");
        let path_id = paths["paths"][0]["id"].as_str().unwrap().to_string();

        let bundle = server
            .bbox_bundle_evidence(Parameters(BundleEvidenceParams {
                question: "What constrains the graph tools?".into(),
                entity_refs: vec![module_ref, invariant_ref],
                path_ids: vec![path_id],
                property_mode: Some("summary".into()),
                include_local_graphs: None,
            }))
            .await;
        let bundle: serde_json::Value = serde_json::from_str(&extract_text(&bundle)).unwrap();
        assert_eq!(bundle["status"], "ok", "{bundle}");
        assert_eq!(bundle["paths"].as_array().unwrap().len(), 1);
        assert!(
            bundle["intra_bundle_edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["kind"] == "repo:CONSTRAINED_BY")
        );

        let deployment_ref = format!("project_graph_vertex:{}:deployments:api", record.project_id);
        let deployment = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: deployment_ref,
                edge_types: Some("ops:DEPLOYED_IN".into()),
                direction: Some("out".into()),
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let deployment: serde_json::Value =
            serde_json::from_str(&extract_text(&deployment)).unwrap();
        assert_eq!(deployment["status"], "ok", "{deployment}");
        assert_eq!(deployment["properties"]["type"], "ops:Service");
    }

    #[tokio::test]
    async fn evidence_bindings_cross_graphs_and_project_files_with_freshness() {
        use bbox_project_graph::{
            EVIDENCE_DOCUMENT_VERSION, EvidenceAssertionAuthority, EvidenceBinding,
            EvidenceDocument,
        };

        let store = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().canonicalize().unwrap();
        write_project_graph_fixture(
            &project_root,
            "records",
            false,
            json!({
                "version": 1,
                "namespace": "record",
                "vertex_types": {
                    "record:Item": {"properties": {"name": "string"}}
                },
                "edge_types": []
            }),
            vec![json!({
                "id": "item-1",
                "type": "record:Item",
                "label": "Item one",
                "properties": {"name": "Item one"}
            })],
            vec![],
        );
        write_project_graph_fixture(
            &project_root,
            "source",
            false,
            json!({
                "version": 1,
                "namespace": "dataset",
                "vertex_types": {
                    "dataset:Asset": {"properties": {"name": "string"}}
                },
                "edge_types": []
            }),
            vec![json!({
                "id": "asset-1",
                "type": "dataset:Asset",
                "label": "Asset one",
                "properties": {"name": "Asset one"}
            })],
            vec![],
        );

        let server = test_server(&store);
        let project_record = server
            .state
            .projects
            .write()
            .register_path(&project_root)
            .unwrap();
        let scope_id = project_record.project_id;
        let record_ref = crate::entity_ref::EntityRef::ProjectGraphVertex {
            scope_id: scope_id.clone(),
            graph_id: "records".into(),
            vertex_id: "item-1".into(),
        };
        let source_ref = crate::entity_ref::EntityRef::ProjectGraphVertex {
            scope_id: scope_id.clone(),
            graph_id: "source".into(),
            vertex_id: "asset-1".into(),
        };
        let file_ref = crate::entity_ref::EntityRef::ProjectFile {
            project_id: scope_id.clone(),
            rel_path_hash: "pathhash".into(),
            chunk_hash: "chunkhash".into(),
            occurrence_idx: 0,
        };
        let evidence = EvidenceDocument {
            version: EVIDENCE_DOCUMENT_VERSION,
            scope_id: scope_id.clone(),
            bindings: vec![
                EvidenceBinding {
                    binding_id: "record-source".into(),
                    scope_id: scope_id.clone(),
                    source: record_ref.clone(),
                    kind: "record:CORRESPONDS_TO".into(),
                    target: source_ref.clone(),
                    assertion_authority: EvidenceAssertionAuthority::Project,
                    observation_id: None,
                    mapping_version: Some("mapping-v1".into()),
                    asserted_at: "2026-01-01T00:00:00Z".into(),
                    source_generation: Some(1),
                    target_generation: Some(1),
                },
                EvidenceBinding {
                    binding_id: "source-file".into(),
                    scope_id: scope_id.clone(),
                    source: source_ref.clone(),
                    kind: "dataset:EVIDENCED_BY".into(),
                    target: file_ref.clone(),
                    assertion_authority: EvidenceAssertionAuthority::Connector,
                    observation_id: Some("observation-file-1".into()),
                    mapping_version: None,
                    asserted_at: "2026-01-01T00:00:00Z".into(),
                    source_generation: Some(1),
                    target_generation: None,
                },
            ],
        };
        let evidence_path = project_root.join(".bbox/evidence/bindings.json");
        fs::create_dir_all(evidence_path.parent().unwrap()).unwrap();
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&evidence).unwrap(),
        )
        .unwrap();

        {
            let idx = server.state.idx.write();
            let fields = idx.field_handles();
            let mut writer = idx.index_handle().writer(50_000_000).unwrap();
            let mut file = tantivy::TantivyDocument::new();
            file.add_text(fields.doc_type, "project_file");
            file.add_text(fields.project_id, &scope_id);
            file.add_text(fields.project, project_root.to_string_lossy());
            file.add_text(fields.file_path, "evidence.txt");
            file.add_text(fields.content, "bounded public fixture evidence");
            file.add_text(fields.entity_id, file_ref.to_string());
            file.add_text(fields.chunk_hash, "chunkhash");
            writer.add_document(file).unwrap();
            writer.commit().unwrap();
            idx.reader_reload_for_test();
        }

        let selector = project_root.to_string_lossy().into_owned();
        for graph_id in ["records", "source"] {
            let result = server
                .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                    project: selector.clone(),
                    graph_id: graph_id.into(),
                    include_local: None,
                }))
                .await;
            let body: serde_json::Value = serde_json::from_str(&extract_text(&result)).unwrap();
            assert_eq!(body["accepted"], true, "{body}");
            assert_eq!(body["evidence_binding_count"], 2, "{body}");
            assert!(body["evidence_error"].is_null(), "{body}");
        }

        let inspected = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: record_ref.to_string(),
                edge_types: Some("record:CORRESPONDS_TO".into()),
                direction: Some("out".into()),
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let inspected: serde_json::Value = serde_json::from_str(&extract_text(&inspected)).unwrap();
        let first_hop = &inspected["edges"]["out"][0];
        assert_eq!(first_hop["target"], source_ref.to_string());
        assert_eq!(first_hop["properties"]["evidence.freshness"], "current");
        assert_eq!(
            first_hop["properties"]["evidence.mapping_version"],
            "mapping-v1"
        );

        let paths = server
            .bbox_find_paths(Parameters(FindPathsParams {
                from: record_ref.to_string(),
                to: Some(file_ref.to_string()),
                to_type: None,
                edge_types: Some(crate::mcp_tools::find_paths::EdgeTypesParam::Many(vec![
                    "record:CORRESPONDS_TO".into(),
                    "dataset:EVIDENCED_BY".into(),
                ])),
                max_depth: Some(2),
                limit: Some(5),
                include_local_graphs: None,
            }))
            .await;
        let paths: serde_json::Value = serde_json::from_str(&extract_text(&paths)).unwrap();
        assert_eq!(paths["paths"].as_array().unwrap().len(), 1, "{paths}");
        let path_id = paths["paths"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(
            paths["paths"][0]["steps"][1]["metadata"]["evidence.freshness"],
            "current"
        );

        let bundle = server
            .bbox_bundle_evidence(Parameters(BundleEvidenceParams {
                question: "How is the record tied to file evidence?".into(),
                entity_refs: vec![
                    record_ref.to_string(),
                    source_ref.to_string(),
                    file_ref.to_string(),
                ],
                path_ids: vec![path_id.clone()],
                property_mode: Some("summary".into()),
                include_local_graphs: None,
            }))
            .await;
        let bundle: serde_json::Value = serde_json::from_str(&extract_text(&bundle)).unwrap();
        assert!(
            bundle["intra_bundle_edges"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| {
                    edge["kind"] == "dataset:EVIDENCED_BY"
                        && edge["properties"]["evidence.observation_id"] == "observation-file-1"
                        && edge["provenance"] == "explicit"
                }),
            "{bundle}"
        );

        let reverse = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: file_ref.to_string(),
                edge_types: Some("dataset:EVIDENCED_BY".into()),
                direction: Some("in".into()),
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let reverse: serde_json::Value = serde_json::from_str(&extract_text(&reverse)).unwrap();
        let reverse_edge = &reverse["edges"]["in"][0];
        assert_eq!(reverse_edge["source"], source_ref.to_string());
        assert_eq!(
            reverse_edge["properties"]["evidence.observation_id"],
            "observation-file-1"
        );
        assert_eq!(
            reverse_edge["properties"]["evidence.target_status"],
            "current"
        );

        fs::write(project_root.join(".bbox/graphs/source/vertices.jsonl"), "").unwrap();
        let descriptor_path = project_root.join(".bbox/graphs/source/graph.json");
        let mut descriptor: serde_json::Value =
            serde_json::from_slice(&fs::read(&descriptor_path).unwrap()).unwrap();
        descriptor["generation"] = json!(2);
        fs::write(
            &descriptor_path,
            serde_json::to_vec_pretty(&descriptor).unwrap(),
        )
        .unwrap();
        let refreshed = server
            .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                project: selector,
                graph_id: "source".into(),
                include_local: None,
            }))
            .await;
        let refreshed: serde_json::Value = serde_json::from_str(&extract_text(&refreshed)).unwrap();
        assert_eq!(refreshed["accepted"], true, "{refreshed}");
        assert_eq!(refreshed["evidence_binding_count"], 2);

        let stale = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: record_ref.to_string(),
                edge_types: Some("record:CORRESPONDS_TO".into()),
                direction: Some("out".into()),
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let stale: serde_json::Value = serde_json::from_str(&extract_text(&stale)).unwrap();
        assert_eq!(
            stale["edges"]["out"][0]["properties"]["evidence.target_status"],
            "stale"
        );
        assert_eq!(
            server
                .state
                .project_graphs
                .read()
                .evidence_bindings(&scope_id)
                .len(),
            2
        );

        let stale_bundle = server
            .bbox_bundle_evidence(Parameters(BundleEvidenceParams {
                question: "Is the retained evidence path still current?".into(),
                entity_refs: vec![record_ref.to_string(), file_ref.to_string()],
                path_ids: vec![path_id],
                property_mode: Some("summary".into()),
                include_local_graphs: None,
            }))
            .await;
        let stale_bundle: serde_json::Value =
            serde_json::from_str(&extract_text(&stale_bundle)).unwrap();
        assert_eq!(
            stale_bundle["paths"][0]["steps"][0]["metadata"]["evidence.target_status"],
            "stale"
        );
        assert_eq!(
            stale_bundle["paths"][0]["steps"][1]["metadata"]["evidence.source_status"],
            "stale"
        );

        let mut invalid_evidence = evidence;
        invalid_evidence.bindings[0].mapping_version = None;
        fs::write(
            &evidence_path,
            serde_json::to_vec_pretty(&invalid_evidence).unwrap(),
        )
        .unwrap();
        let rejected = server
            .bbox_project_graph_validate(Parameters(ProjectGraphExactParams {
                project: project_root.to_string_lossy().into_owned(),
                graph_id: "records".into(),
                include_local: None,
            }))
            .await;
        let rejected: serde_json::Value = serde_json::from_str(&extract_text(&rejected)).unwrap();
        assert_eq!(rejected["accepted"], true, "{rejected}");
        assert!(rejected["evidence_error"].is_string(), "{rejected}");
        assert_eq!(rejected["evidence_binding_count"], 2);
        assert_eq!(
            server
                .state
                .project_graphs
                .read()
                .evidence_bindings(&scope_id)
                .len(),
            2,
            "invalid candidate must not replace the prior accepted evidence set"
        );
    }

    #[tokio::test]
    async fn scratch_graphs_are_excluded_by_default_on_tool_surfaces() {
        let store = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let project_root = project.path().canonicalize().unwrap();
        write_project_graph_fixture(
            &project_root,
            "scratch",
            true,
            json!({
                "version": 1,
                "namespace": "scratch",
                "vertex_types": {
                    "scratch:Note": {"properties": {"text": "string"}}
                },
                "edge_types": []
            }),
            vec![json!({
                "id": "note-1",
                "type": "scratch:Note",
                "label": "scratch note",
                "properties": {"text": "local only"}
            })],
            vec![],
        );
        let server = test_server(&store);
        let record = server
            .state
            .projects
            .write()
            .register_path(&project_root)
            .unwrap();
        let selector = project_root.to_string_lossy().into_owned();

        let default_list = server
            .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                project: Some(selector.clone()),
                include_local: None,
            }))
            .await;
        let default_list: serde_json::Value =
            serde_json::from_str(&extract_text(&default_list)).unwrap();
        assert!(default_list["graphs"].as_array().unwrap().is_empty());

        let local_list = server
            .bbox_project_graph_list(Parameters(ProjectGraphListParams {
                project: Some(selector),
                include_local: Some(true),
            }))
            .await;
        let local_list: serde_json::Value =
            serde_json::from_str(&extract_text(&local_list)).unwrap();
        assert_eq!(local_list["graphs"].as_array().unwrap().len(), 1);

        let note_ref = format!("project_graph_vertex:{}:scratch:note-1", record.project_id);
        let excluded = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: note_ref.clone(),
                edge_types: None,
                direction: None,
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: None,
            }))
            .await;
        let excluded: serde_json::Value = serde_json::from_str(&extract_text(&excluded)).unwrap();
        assert_eq!(excluded["status"], "error.invalid_project_graph");

        let included = server
            .bbox_inspect_entity(Parameters(InspectEntityParams {
                entity_ref: note_ref,
                edge_types: None,
                direction: None,
                per_type_limit: Some(5),
                property_mode: Some("full".into()),
                include_local_graphs: Some(true),
            }))
            .await;
        let included: serde_json::Value = serde_json::from_str(&extract_text(&included)).unwrap();
        assert_eq!(included["status"], "ok", "{included}");
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
        *server.state.edge_index.write() = EdgeIndex::from_edges_for_tests(vec![Edge {
            source: crate::entity_ref::EntityRef::parse(symbol).unwrap(),
            kind: "DEFINED_IN".into(),
            target: crate::entity_ref::EntityRef::parse(file).unwrap(),
            provenance: EdgeProvenance::Derived,
            confidence: EdgeConfidence::Exact,
            metadata: Default::default(),
        }]);

        let inspect = |entity_ref: String| {
            let server = server.clone();
            async move {
                let result = server
                    .bbox_inspect_entity(Parameters(InspectEntityParams {
                        entity_ref,
                        edge_types: None,
                        direction: None,
                        per_type_limit: Some(5),
                        property_mode: Some("full".into()),
                        include_local_graphs: None,
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
                        edge_types: None,
                        direction: None,
                        per_type_limit: Some(5),
                        property_mode: Some("full".into()),
                        include_local_graphs: None,
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
}

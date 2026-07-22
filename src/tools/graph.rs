use crate::mcp_tools;
use crate::mcp_tools::blame::BlameParams;
use crate::mcp_tools::bundle_evidence::BundleEvidenceParams;
use crate::mcp_tools::describe_schema::DescribeSchemaOptions;
use crate::mcp_tools::find_paths::FindPathsParams;
use crate::mcp_tools::inspect::InspectEntityParams;
use crate::mcp_tools::provenance::ProvenanceParams;
use crate::mcp_tools::provenance_plan::ProvenanceExportPlanParams;
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
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
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
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
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
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
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
        description = "Measure the byte payload size of entity refs. file refs resolve against optional project_dir first, then registered project file content; project_file and project_file_v2 refs resolve to full indexed chunk content; other refs resolve through entity providers and measure serialized provider-properties JSON. Accepts up to 500 refs; successful refs are canonicalized and unresolved/omitted refs are reported under degraded."
    )]
    pub(crate) async fn bbox_ref_size(
        &self,
        Parameters(p): Parameters<RefSizeParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_ref_size", move || {
            let read_view = server.state.code_read_view.read().clone();
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(edge_index)
                    .with_searcher(&read_view.searcher);
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
            let read_view = server.state.code_read_view.read().clone();
            let edge_index = read_view.edge_index.as_ref();
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref())
                    .with_edge_index(edge_index)
                    .with_searcher(&read_view.searcher);
            let projects = server.state.projects.read().list();
            mcp_tools::blame::blame(&p, &provider_ctx, edge_index, &projects)
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
            let read_view = server.state.code_read_view.read().clone();
            mcp_tools::provenance::export_provenance(&p, read_view.edge_index.as_ref(), &projects)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts;
    use crate::server::state::SharedState;
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
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

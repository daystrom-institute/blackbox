use crate::index::{CiteParams, ContextParams, ProjectFilterInput, SearchParams};
use crate::mcp_tools;
use crate::mcp_tools::discover_seed::DiscoverSeedParams;
use crate::mcp_tools::hybrid_search::HybridSearchParams;
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::json;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct CorpusSearchParams {
    /// Search query.
    query: String,
    /// Maximum hits to return (default 10, max 100).
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::transcripts_tools()
}

/// Filter-class boundary for the corpus-search family (`bbox_search`,
/// `bbox_cite`, `bbox_sessions_list`, `work_tool_calls`): resolve the raw
/// selector once here and hand the index engine a typed filter. The
/// literal travels unchanged so the substring lane keeps its semantics;
/// the `base_project_id` term lane fires only when the selector resolved
/// to a registered project.
pub(crate) fn corpus_project_filter(
    server: &BlackboxServer,
    raw: Option<&str>,
) -> Option<ProjectFilterInput> {
    raw.map(|literal| ProjectFilterInput {
        project_id: server
            .resolve_project_filter(literal)
            .and_then(|resolution| resolution.project_id().map(str::to_owned)),
        literal: literal.to_string(),
    })
}

#[tool_router(router = transcripts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_corpus_search",
        description = "Compatibility corpus lookup for harness capability projection. Returns ranked hits with stable id/text fields."
    )]
    pub(crate) async fn bbox_corpus_search(
        &self,
        Parameters(p): Parameters<CorpusSearchParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_corpus_search", move || {
            let query = p.query.trim();
            anyhow::ensure!(!query.is_empty(), "`query` is required");
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .index_writer
                    .run_reindex_pass(false, true)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let read_view = server.state.code_read_view.read().clone();
            let hits = server
                .state
                .idx
                .read()
                .hybrid_bm25_hits_filtered_with_active_selectors_and_searcher(
                    query,
                    p.limit.unwrap_or(10).clamp(1, 100),
                    None,
                    false,
                    &read_view.active_selectors,
                    &read_view.searcher,
                )?;
            Ok(serde_json::to_string(&json!({
                "hits": hits
                    .into_iter()
                    .map(|hit| json!({
                        "id": hit.entity_id,
                        "text": match hit.title {
                            Some(title) => format!("{title}\n{}", hit.excerpt),
                            None => hit.excerpt,
                        },
                    }))
                    .collect::<Vec<_>>(),
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_search",
        description = "Search across all indexed transcripts. Default `mode=smart` broadens adjacent terms for recall; `mode=fulltext` gives raw Tantivy/Lucene-style boolean syntax."
    )]
    pub(crate) async fn bbox_search(
        &self,
        Parameters(p): Parameters<SearchParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_search", move || {
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .index_writer
                    .run_reindex_pass(false, true)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let project_filter = corpus_project_filter(&server, p.project.as_deref());
            let read_view = server.state.code_read_view.read().clone();
            server.state.idx.read().search_with_project_filter(
                &p,
                project_filter.as_ref(),
                &read_view.active_selectors,
                &read_view.searcher,
            )
        })
        .await
    }

    #[tool(
        name = "bbox_hybrid_search",
        description = "Hybrid BM25+vector search over typed entities. vector_weight=0.6 by default; set 0.0 for BM25-only behavior, 1.0 for vector-only."
    )]
    pub(crate) async fn bbox_hybrid_search(
        &self,
        Parameters(p): Parameters<HybridSearchParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_hybrid_search", move || {
            let mut p = p;
            p.resolved_project_id =
                server.resolve_hybrid_project_filter("bbox_hybrid_search", p.project.as_deref());
            // Fast path: read-lock the index to check emptiness. Only escalate
            // to a write lock if we actually need to build_index. The previous
            // unconditional write lock blocked every search behind the
            // auto-reindex thread's writer, adding 5-30 seconds of latency
            // to interactive queries during reindex windows.
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .index_writer
                    .run_reindex_pass(false, true)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let knowledge_view =
                server.session_knowledge_view(p.project.as_deref(), p.provisional.as_deref())?;
            let read_view = server.state.code_read_view.read().clone();
            let provider_ctx = server
                .provider_context()
                .with_knowledge_view(&knowledge_view.knowledge)
                .with_searcher(&read_view.searcher);
            let response =
                mcp_tools::hybrid_search::hybrid_search_typed_with_active_selectors_and_searcher(
                    &server.state.idx.read(),
                    &knowledge_view.knowledge,
                    &provider_ctx,
                    &p,
                    &read_view.active_selectors,
                    &read_view.searcher,
                )?;
            knowledge_view.enrich_json_response(serde_json::to_string(&response)?)
        })
        .await
    }

    #[tool(
        name = "bbox_discover_seed_entities",
        description = "Find seed entities with notable_edges; inspect before answering."
    )]
    pub(crate) async fn bbox_discover_seed_entities(
        &self,
        Parameters(p): Parameters<DiscoverSeedParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_discover_seed_entities", move || {
            let read_view = server.state.complete_code_read_view()?;
            let mut p = p;
            p.resolved_project_id = server
                .resolve_hybrid_project_filter("bbox_discover_seed_entities", p.project.as_deref());
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .index_writer
                    .run_reindex_pass(false, true)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let knowledge_view =
                server.session_knowledge_view(p.project.as_deref(), p.provisional.as_deref())?;
            let provider_ctx = server
                .provider_context()
                .with_knowledge_view(&knowledge_view.knowledge)
                .with_searcher(&read_view.searcher);
            let output = mcp_tools::discover_seed::discover_seed_entities(
                &server.state.idx.read(),
                &knowledge_view.knowledge,
                &provider_ctx,
                read_view.edge_index.as_ref(),
                &read_view.active_selectors,
                &read_view.searcher,
                &p,
            )?;
            knowledge_view.enrich_json_response(output)
        })
        .await
    }

    #[tool(
        name = "bbox_cite",
        description = "Trace a claim back to the turn that established it."
    )]
    pub(crate) async fn bbox_cite(&self, Parameters(p): Parameters<CiteParams>) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_cite", move || {
            let project_filter = corpus_project_filter(&server, p.project.as_deref());
            server.state.idx.read().cite(&p, project_filter.as_ref())
        })
        .await
    }

    #[tool(
        name = "bbox_context",
        description = "Conversation context around a specific byte offset."
    )]
    pub(crate) async fn bbox_context(
        &self,
        Parameters(p): Parameters<ContextParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_context", move || server.state.idx.read().context(&p)).await
    }
}

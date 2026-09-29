use crate::index::{ContextParams, ProjectFilterInput, SearchParams};
use crate::mcp_tools;
use crate::mcp_tools::hybrid_search::HybridSearchParams;
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::Deserialize;

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub(crate) struct ContextToolParams {
    #[serde(flatten)]
    pub selection: ContextParams,
    /// Exact stored native record page. Requires an indexed-transcript handle
    /// from an exact_read hint; omit context_lines. Maximum 4096 bytes.
    #[serde(default)]
    pub body_limit: Option<usize>,
    /// Continue body.next_cursor with the same handle and event offset.
    #[serde(default)]
    pub body_cursor: Option<String>,
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::transcripts_tools()
}

/// Filter-class boundary for the corpus-search family (`bbox_search`,
/// `bbox_sessions_list`, `bbox_tool_calls`): resolve the raw
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
        description = "Search typed entities with BM25 and vectors. Returns bounded evidence hits and retrieval status. Use debug for ranking diagnostics."
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
            let graph_policy = server.graph_word_policy_snapshot();
            let response =
                mcp_tools::hybrid_search::hybrid_search_typed_with_active_selectors_and_searcher(
                    &server.state.idx.read(),
                    &knowledge_view.knowledge,
                    &provider_ctx,
                    &p,
                    &read_view.active_selectors,
                    &read_view.searcher,
                    Some(&graph_policy),
                )?;
            knowledge_view.enrich_json_response(serde_json::to_string(&response)?)
        })
        .await
    }

    #[tool(
        name = "bbox_context",
        description = "Read surrounding indexed events by opaque locator and offset, or page an exact native stored record using its recovery handle. Native replies disclose projection and freshness limits."
    )]
    pub(crate) async fn bbox_context(
        &self,
        Parameters(p): Parameters<ContextToolParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_context", move || {
            if p.body_limit.is_some() || p.body_cursor.is_some() {
                anyhow::ensure!(
                    p.selection.context_lines.is_none(),
                    "exact record pages do not accept context_lines"
                );
                server.state.idx.read().native_reader_detail(
                    &p.selection.file_path,
                    p.selection.byte_offset,
                    p.body_cursor.as_deref(),
                    p.body_limit,
                )
            } else {
                server.state.idx.read().context(&p.selection)
            }
        })
        .await
    }
}

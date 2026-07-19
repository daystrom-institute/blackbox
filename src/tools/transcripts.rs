use crate::index::{CiteParams, ContextParams, SearchParams};
use crate::mcp_tools;
use crate::mcp_tools::discover_seed::DiscoverSeedParams;
use crate::mcp_tools::hybrid_search::HybridSearchParams;
use crate::providers::ProviderContext;
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
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let hits = server.state.idx.read().hybrid_bm25_hits(
                query,
                p.limit.unwrap_or(10).clamp(1, 100),
                None,
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
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            server.state.idx.read().search(&p)
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
        Self::run_blocking("bbox_hybrid_search", move || {
            // Fast path: read-lock the index to check emptiness. Only escalate
            // to a write lock if we actually need to build_index. The previous
            // unconditional write lock blocked every search behind the
            // auto-reindex thread's writer, adding 5-30 seconds of latency
            // to interactive queries during reindex windows.
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref());
            mcp_tools::hybrid_search::hybrid_search(
                &server.state.idx.read(),
                &server.state.kb.read(),
                &provider_ctx,
                &p,
            )
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
        Self::run_blocking("bbox_discover_seed_entities", move || {
            if server.state.idx.read().is_empty() {
                server
                    .state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let provider_ctx =
                ProviderContext::new_with_ext(server.state.corpus_stores(), server.state.as_ref());
            mcp_tools::discover_seed::discover_seed_entities(
                &server.state.idx.read(),
                &server.state.kb.read(),
                &provider_ctx,
                &server.state.edge_index.read(),
                &p,
            )
        })
        .await
    }

    #[tool(
        name = "bbox_cite",
        description = "Trace a claim back to the turn that established it."
    )]
    pub(crate) async fn bbox_cite(&self, Parameters(p): Parameters<CiteParams>) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_cite", move || server.state.idx.read().cite(&p)).await
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

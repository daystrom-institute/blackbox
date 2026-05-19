use crate::index::{CiteParams, ContextParams, SearchParams};
use crate::mcp_tools;
use crate::mcp_tools::discover_seed::DiscoverSeedParams;
use crate::mcp_tools::hybrid_search::HybridSearchParams;
use crate::providers::ProviderContext;
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::transcripts_tools()
}

#[tool_router(router = transcripts_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_search",
        description = "Search across all indexed transcripts. Default `mode=smart` broadens adjacent terms for recall; `mode=fulltext` gives raw Tantivy/Lucene-style boolean syntax."
    )]
    pub(crate) fn bbox_search(&self, Parameters(p): Parameters<SearchParams>) -> CallToolResult {
        Self::run("bbox_search", || {
            if self.state.idx.read().is_empty() {
                self.state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            self.state.idx.read().search(&p)
        })
    }

    #[tool(
        name = "bbox_hybrid_search",
        description = "Hybrid BM25+vector search over typed entities. vector_weight=0.6 by default; set 0.0 for BM25-only behavior, 1.0 for vector-only."
    )]
    pub(crate) fn bbox_hybrid_search(
        &self,
        Parameters(p): Parameters<HybridSearchParams>,
    ) -> CallToolResult {
        Self::run("bbox_hybrid_search", || {
            // Fast path: read-lock the index to check emptiness. Only escalate
            // to a write lock if we actually need to build_index. The previous
            // unconditional write lock blocked every search behind the
            // auto-reindex thread's writer, adding 5-30 seconds of latency
            // to interactive queries during reindex windows.
            if self.state.idx.read().is_empty() {
                self.state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::hybrid_search::hybrid_search(
                &self.state.idx.read(),
                &self.state.kb.read(),
                &provider_ctx,
                &p,
            )
        })
    }

    #[tool(
        name = "bbox_discover_seed_entities",
        description = "Find seed entities with notable_edges; inspect before answering."
    )]
    pub(crate) fn bbox_discover_seed_entities(
        &self,
        Parameters(p): Parameters<DiscoverSeedParams>,
    ) -> CallToolResult {
        Self::run("bbox_discover_seed_entities", || {
            if self.state.idx.read().is_empty() {
                self.state
                    .idx
                    .write()
                    .build_index(false)
                    .map_err(|e| anyhow::anyhow!("Auto-index failed: {e}"))?;
            }
            let provider_ctx = ProviderContext::new(&self.state);
            mcp_tools::discover_seed::discover_seed_entities(
                &self.state.idx.read(),
                &self.state.kb.read(),
                &provider_ctx,
                &self.state.edge_index.read(),
                &p,
            )
        })
    }

    #[tool(
        name = "bbox_cite",
        description = "Trace a claim back to the turn that established it."
    )]
    pub(crate) fn bbox_cite(&self, Parameters(p): Parameters<CiteParams>) -> CallToolResult {
        Self::run("bbox_cite", || self.state.idx.read().cite(&p))
    }

    #[tool(
        name = "bbox_context",
        description = "Conversation context around a specific byte offset."
    )]
    pub(crate) fn bbox_context(&self, Parameters(p): Parameters<ContextParams>) -> CallToolResult {
        Self::run("bbox_context", || self.state.idx.read().context(&p))
    }
}

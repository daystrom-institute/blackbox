use crate::server::BlackboxServer;
use crate::{embed, embed_queue};
use crate::embed::ReembedParams;
use crate::index::{
    MessagesParams, ReindexParams, SessionParams, SessionsListParams, TopicsParams,
};

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::sessions_tools()
}

#[tool_router(router = sessions_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_session",
        description = "Summary metadata for a single session."
    )]
    pub(crate) fn bbox_session(&self, Parameters(p): Parameters<SessionParams>) -> CallToolResult {
        Self::run("bbox_session", || self.state.idx.read().session(&p))
    }

    #[tool(
        name = "bbox_messages",
        description = "Chronological messages from a session."
    )]
    pub(crate) fn bbox_messages(
        &self,
        Parameters(p): Parameters<MessagesParams>,
    ) -> CallToolResult {
        Self::run("bbox_messages", || self.state.idx.read().messages(&p))
    }

    #[tool(
        name = "bbox_reindex",
        description = "Build or incrementally update the search index."
    )]
    pub(crate) fn bbox_reindex(&self, Parameters(p): Parameters<ReindexParams>) -> CallToolResult {
        Self::run("bbox_reindex", || self.state.idx.write().reindex(&p))
    }

    #[tool(
        name = "bbox_reembed",
        description = "Request an embedding rebuild for a configured route."
    )]
    pub(crate) fn bbox_reembed(&self, Parameters(p): Parameters<ReembedParams>) -> CallToolResult {
        let state = self.state.clone();
        Self::run("bbox_reembed", || embed::reembed_start(&p, state))
    }

    #[tool(
        name = "bbox_embed_status",
        description = "Return per-route embedding queue health."
    )]
    pub(crate) fn bbox_embed_status(&self) -> CallToolResult {
        Self::run("bbox_embed_status", || {
            embed_queue::status_json_for_state(&self.state)
        })
    }

    #[tool(
        name = "bbox_topics",
        description = "Top terms in a session by frequency."
    )]
    pub(crate) fn bbox_topics(&self, Parameters(p): Parameters<TopicsParams>) -> CallToolResult {
        Self::run("bbox_topics", || self.state.idx.read().topics(&p))
    }

    #[tool(
        name = "bbox_sessions_list",
        description = "Browse sessions sorted by recency."
    )]
    pub(crate) fn bbox_sessions_list(
        &self,
        Parameters(p): Parameters<SessionsListParams>,
    ) -> CallToolResult {
        Self::run("bbox_sessions_list", || {
            self.state.idx.read().sessions_list(&p)
        })
    }

    #[tool(
        name = "bbox_stats",
        description = "Corpus statistics (doc count, index size, file counts)."
    )]
    pub(crate) fn bbox_stats(&self) -> CallToolResult {
        Self::run("bbox_stats", || self.state.idx.read().stats())
    }
}

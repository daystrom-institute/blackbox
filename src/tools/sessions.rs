use crate::embed_runtime::ReembedParams;
use crate::index::{
    MessagesParams, ReindexParams, SessionParams, SessionsListParams, TopicsParams,
};
use crate::server::BlackboxServer;

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
    pub(crate) async fn bbox_session(
        &self,
        Parameters(p): Parameters<SessionParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_session", move || server.state.idx.read().session(&p)).await
    }

    #[tool(
        name = "bbox_messages",
        description = "Chronological messages from a session."
    )]
    pub(crate) async fn bbox_messages(
        &self,
        Parameters(p): Parameters<MessagesParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_messages", move || {
            server.state.idx.read().messages(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_reindex",
        description = "Build or incrementally update the search index."
    )]
    pub(crate) async fn bbox_reindex(
        &self,
        Parameters(p): Parameters<ReindexParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_reindex", move || {
            // Runs on the writer actor: unified with the background pass
            // (which also picks up thread/roadmap store docs the old manual
            // path skipped) and never contends for the writer lock.
            server
                .state
                .index_writer
                .run_reindex_pass(p.full.unwrap_or(false), true)
        })
        .await
    }

    #[tool(
        name = "bbox_reembed",
        description = "Request an embedding rebuild for a configured route."
    )]
    pub(crate) async fn bbox_reembed(
        &self,
        Parameters(p): Parameters<ReembedParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_reembed", move || {
            crate::embed_runtime::reembed_start(&p, server.state)
        })
        .await
    }

    #[tool(
        name = "bbox_embed_status",
        description = "Return route embedding health and health_reason."
    )]
    pub(crate) fn bbox_embed_status(&self) -> CallToolResult {
        Self::run("bbox_embed_status", || {
            crate::embed_runtime::status_json_for_state(&self.state)
        })
    }

    #[tool(
        name = "bbox_topics",
        description = "Top terms in a session by frequency."
    )]
    pub(crate) async fn bbox_topics(
        &self,
        Parameters(p): Parameters<TopicsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_topics", move || server.state.idx.read().topics(&p)).await
    }

    #[tool(
        name = "bbox_sessions_list",
        description = "Browse sessions sorted by recency."
    )]
    pub(crate) async fn bbox_sessions_list(
        &self,
        Parameters(p): Parameters<SessionsListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_sessions_list", move || {
            server.state.idx.read().sessions_list(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_stats",
        description = "Corpus statistics (doc count, index size, file counts)."
    )]
    pub(crate) async fn bbox_stats(&self) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_stats", move || server.state.idx.read().stats()).await
    }
}

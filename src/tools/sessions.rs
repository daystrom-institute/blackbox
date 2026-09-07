use crate::embed_runtime::{EmbedPartitionsParams, ReembedParams};
use crate::index::{
    MessagesParams, ReindexParams, SessionParams, SessionsListParams, TopicsParams,
};
use crate::server::BlackboxServer;

use crate::embed_runtime::status_snapshot::EmbedStatusParams;
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
        description = "Page stored messages by exact session ID or opaque transcript locator. Native replies disclose projection and freshness limits."
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
        description = "Queue a full or incremental search-index update. Returns after admission by default; wait=true is for internal migrations that require completion."
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
            // `accept_empty_projects` is operator authority passed straight
            // through (RX-V1): never defaulted, never inferred from a prior
            // refusal.
            let full = p.full.unwrap_or(false);
            let accepted_empty = p.accept_empty_projects.unwrap_or_default();
            if p.wait.unwrap_or(false) {
                server.state.index_writer.run_reindex_pass_accepting_empty(
                    full,
                    true,
                    accepted_empty,
                )
            } else {
                server
                    .state
                    .index_writer
                    .request_reindex_pass_accepting_empty(full, true, accepted_empty)
            }
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
        name = "bbox_embed_partitions",
        description = "Vector partition lifecycle on daemon-owned storage: list partitions with route mapping, dims, dtype, compatibility family, active_count, last_write (paged; limit default 20, max 100); prune orphaned partitions; scrub misattributed vectors from a mapped partition (dry-run default)."
    )]
    pub(crate) async fn bbox_embed_partitions(
        &self,
        Parameters(p): Parameters<EmbedPartitionsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_embed_partitions", move || {
            crate::embed_runtime::embed_partitions(&p, &server.state)
        })
        .await
    }

    #[tool(
        name = "bbox_embed_status",
        description = "Read embedding health. Scan and probe opt-ins can be expensive; oversized reports use session snapshots with exact cursor recovery."
    )]
    pub(crate) async fn bbox_embed_status(
        &self,
        Parameters(p): Parameters<EmbedStatusParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_embed_status", move || {
            crate::embed_runtime::status_snapshot::read_status(
                &server.embed_status_snapshots,
                &p,
                || crate::embed_runtime::status_snapshot::collect_status(&server.state, &p),
            )
        })
        .await
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
        description = "Browse configured provider session metadata by recency. limit defaults to 30, maximum 100; use offset to continue. project filters by registered project identity or recorded path text. Empty means no matching metadata at this offset; corpus search may still find transcripts."
    )]
    pub(crate) async fn bbox_sessions_list(
        &self,
        Parameters(p): Parameters<SessionsListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_sessions_list", move || {
            let project_filter =
                crate::tools::transcripts::corpus_project_filter(&server, p.project.as_deref());
            server
                .state
                .idx
                .read()
                .sessions_list(&p, project_filter.as_ref())
        })
        .await
    }

    #[tool(
        name = "bbox_stats",
        description = "Indexed document and segment counts, cached up to 60s."
    )]
    pub(crate) async fn bbox_stats(&self) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_stats", move || server.state.idx.read().stats()).await
    }
}

#[cfg(test)]
mod tests {
    use super::EmbedStatusParams;

    #[test]
    fn embed_status_coverage_scan_is_opt_in() {
        let default: EmbedStatusParams = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(default.include_coverage, None);

        let requested: EmbedStatusParams =
            serde_json::from_value(serde_json::json!({"include_coverage": true})).unwrap();
        assert_eq!(requested.include_coverage, Some(true));
    }
}

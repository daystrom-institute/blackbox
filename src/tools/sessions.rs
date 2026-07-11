use crate::embed_runtime::{EmbedPartitionsParams, ReembedParams};
use crate::index::{
    MessagesParams, ReindexParams, SessionParams, SessionsListParams, TopicsParams,
};
use crate::server::BlackboxServer;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::sessions_tools()
}

#[derive(Debug, Default, Serialize, Deserialize, schemars::JsonSchema)]
pub(crate) struct EmbedStatusParams {
    /// Optional vector route (partition name, e.g. "voyage-1024") to run a
    /// sampled HNSW self-recall probe against (gap-1168b0bd c). The probe is
    /// O(sample × search) — seconds on large partitions — and errors with
    /// "busy" if the partition is mid-rebuild instead of blocking.
    #[serde(default)]
    pub recall_probe_route: Option<String>,
    /// Probe every Nth active vector (default 50). Lower is more accurate
    /// and proportionally slower.
    #[serde(default)]
    pub probe_sample_every: Option<usize>,
    /// Top-k window the probed vector must appear in (default 10).
    #[serde(default)]
    pub probe_k: Option<usize>,
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
        name = "bbox_embed_partitions",
        description = "Vector partition lifecycle: list partitions with route mapping, dims, dtype, compatibility family, active_count, last_write; prune orphaned partitions; scrub misattributed vectors from a mapped partition (dry-run default)."
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
        description = "Return route embedding health and health_reason. recall_probe_route runs a sampled HNSW self-recall probe against that vector partition (vector-recall diagnostic, seconds on large partitions)."
    )]
    pub(crate) async fn bbox_embed_status(
        &self,
        Parameters(p): Parameters<EmbedStatusParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_embed_status", move || {
            let status = crate::embed_runtime::status_json_for_state(&server.state)?;
            let Some(route) = p.recall_probe_route.as_deref() else {
                return Ok(status);
            };
            let sample_every = p.probe_sample_every.unwrap_or(50).max(1);
            let k = p.probe_k.unwrap_or(10).max(1);
            let self_recall = crate::vectors::self_recall_probe(route, sample_every, k)?;
            let mut value: serde_json::Value = serde_json::from_str(&status)?;
            value["recall_probe"] = serde_json::json!({
                "route": route,
                "sample_every": sample_every,
                "k": k,
                // null = partition exists but has no HNSW graph yet (or the
                // store is still warming up). A healthy graph scores ~1.0;
                // reverse-edge orphaning drags this down (gap-2eabd96d).
                "self_recall": self_recall,
            });
            Ok(serde_json::to_string_pretty(&value)?)
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

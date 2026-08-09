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
    /// Compute exact embedding coverage by walking every source document.
    /// Disabled by default because a production corpus scan can take minutes;
    /// queue/provider health remains available on the cheap path.
    #[serde(default)]
    pub include_coverage: Option<bool>,
    /// Include explicit HNSW graph diagnostics. Cheap status leaves graph
    /// fields absent; this opt-in walk can be expensive on large partitions.
    #[serde(default)]
    pub include_diagnostics: Option<bool>,
    /// Optional bounded vector-route set for graph diagnostics. When omitted,
    /// at most 64 currently loaded routes are inspected.
    #[serde(default)]
    pub diagnostic_routes: Option<Vec<String>>,
    /// Cooperative graph-diagnostic deadline in milliseconds (default 2000,
    /// clamped to 1..=30000).
    #[serde(default)]
    pub diagnostic_deadline_ms: Option<u64>,
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
        description = "Return cheap route embedding health and health_reason. include_coverage explicitly requests a full source-corpus coverage scan; include_diagnostics requests deadline-bounded HNSW graph diagnostics; recall_probe_route runs a sampled self-recall probe (all opt-ins can be expensive)."
    )]
    pub(crate) async fn bbox_embed_status(
        &self,
        Parameters(p): Parameters<EmbedStatusParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_embed_status", move || {
            let status = crate::embed_runtime::status_json_for_state(
                &server.state,
                p.include_coverage.unwrap_or(false),
            )?;
            let diagnostics_requested =
                p.include_diagnostics.unwrap_or(false) || p.diagnostic_routes.is_some();
            if !diagnostics_requested && p.recall_probe_route.is_none() {
                return Ok(status);
            }
            let mut value: serde_json::Value = serde_json::from_str(&status)?;
            if diagnostics_requested {
                let routes = match p.diagnostic_routes.as_ref() {
                    Some(routes) => routes.clone(),
                    None => crate::vectors::try_metrics()
                        .map(|metrics| metrics.into_keys().take(64).collect())
                        .unwrap_or_default(),
                };
                let timeout = std::time::Duration::from_millis(
                    p.diagnostic_deadline_ms.unwrap_or(2_000).clamp(1, 30_000),
                );
                value["vector_diagnostics"] =
                    match crate::vectors::try_diagnostics_bounded(&routes, timeout) {
                        Some(report) => serde_json::to_value(report?)?,
                        None => serde_json::json!({
                            "partitions": {},
                            "unavailable": [{
                                "route": "<vector-store>",
                                "reason": "store_warming_up"
                            }]
                        }),
                    };
            }
            if let Some(route) = p.recall_probe_route.as_deref() {
                let sample_every = p.probe_sample_every.unwrap_or(50).max(1);
                let k = p.probe_k.unwrap_or(10).max(1);
                let self_recall = crate::vectors::self_recall_probe(route, sample_every, k)?;
                value["recall_probe"] = serde_json::json!({
                    "route": route,
                    "sample_every": sample_every,
                    "k": k,
                    // null = partition exists but has no HNSW graph yet (or the
                    // store is still warming up). A healthy graph scores ~1.0;
                    // reverse-edge orphaning drags this down (gap-2eabd96d).
                    "self_recall": self_recall,
                });
            }
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
        description = "Corpus statistics (doc count, index size, file counts)."
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

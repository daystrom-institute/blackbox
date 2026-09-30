use crate::index::{ContextParams, ProjectFilterInput};
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    async fn hybrid(server: &BlackboxServer, arguments: Value) -> Value {
        let result = server
            .bbox_hybrid_search(Parameters(
                serde_json::from_value::<HybridSearchParams>(arguments).unwrap(),
            ))
            .await;
        assert_ne!(result.is_error, Some(true), "{result:?}");
        let result = serde_json::to_value(result).unwrap();
        serde_json::from_str(result["content"][0]["text"].as_str().unwrap()).unwrap()
    }

    fn entity_ids(response: &Value) -> Vec<String> {
        response["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["entity_id"].as_str().unwrap().to_string())
            .collect()
    }

    /// Conversation search runs through bbox_hybrid_search: document filters
    /// narrow every lane, fulltext mode takes boolean syntax, and a
    /// conversation hit's coordinates page the exact stored record through
    /// bbox_context.
    #[tokio::test]
    async fn hybrid_conversation_filters_and_coordinates_round_trip_through_context() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(&root),
        ));
        {
            let index = server.state.idx.read();
            let fields = index.field_handles();
            let mut writer = index
                .index_handle()
                .writer::<tantivy::TantivyDocument>(15_000_000)
                .unwrap();
            for (offset, role, word) in [(0u64, "user", "alpha"), (1, "assistant", "beta")] {
                let mut doc = tantivy::TantivyDocument::new();
                doc.add_text(fields.doc_type, "transcript");
                doc.add_text(fields.content, format!("hybridfoldneedle {word}"));
                doc.add_text(fields.file_path, "native:synthetic/fold-stream");
                doc.add_text(fields.session_id, "fold-session");
                doc.add_text(fields.account, "codex");
                doc.add_text(fields.source, "codex");
                doc.add_text(fields.project, "/synthetic/fold-project");
                doc.add_text(fields.timestamp, "2026-09-01T00:00:00Z");
                doc.add_text(fields.role, role);
                doc.add_u64(fields.is_subagent, 0);
                doc.add_u64(fields.byte_offset, offset);
                writer.add_document(doc).unwrap();
            }
            let mut commit = tantivy::TantivyDocument::new();
            commit.add_text(fields.doc_type, "commit");
            commit.add_text(fields.entity_id, "commit:fold1234:abcdef1234567890");
            commit.add_text(fields.content, "hybridfoldneedle gamma");
            commit.add_text(fields.role, "commit");
            writer.add_document(commit).unwrap();
            writer.commit().unwrap();
            index.reader_reload_for_test();
        }
        // The fixture writes the index directly, so it repins the read view
        // the way a writer-actor commit would.
        let current = server.state.code_read_view.read().clone();
        *server.state.code_read_view.write() =
            std::sync::Arc::new(crate::server::state::CodeReadView {
                active_selectors: current.active_selectors.clone(),
                searcher: server.state.idx.read().searcher(),
                catalog_epoch: current.catalog_epoch,
                git_overlays: current.git_overlays.clone(),
            });
        let lexical = json!({"include_vectors": false, "rerank": "none"});
        let with = |extra: Value| {
            let mut arguments = lexical.clone();
            arguments
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            arguments
        };

        let all = hybrid(&server, with(json!({"query": "hybridfoldneedle"}))).await;
        assert_eq!(entity_ids(&all).len(), 3, "{all}");

        let users = hybrid(
            &server,
            with(json!({"query": "hybridfoldneedle", "role": "user"})),
        )
        .await;
        assert_eq!(
            entity_ids(&users),
            vec!["transcript:codex:fold-session:0:0"],
            "{users}"
        );
        let coordinates = &users["results"][0]["conversation"];
        assert_eq!(coordinates["session_id"], "fold-session");
        assert_eq!(coordinates["byte_offset"], 0);
        assert!(
            users["next_steps"][0]
                .as_str()
                .unwrap()
                .contains("bbox_context("),
            "{users}"
        );

        let outside = hybrid(
            &server,
            with(json!({
                "query": "hybridfoldneedle", "doc_type": "transcript",
                "project": "/synthetic/elsewhere",
            })),
        )
        .await;
        assert!(entity_ids(&outside).is_empty(), "{outside}");
        let inside = hybrid(
            &server,
            with(json!({
                "query": "hybridfoldneedle", "doc_type": "transcript",
                "project": "/synthetic/fold-project",
            })),
        )
        .await;
        assert_eq!(entity_ids(&inside).len(), 2, "{inside}");

        let fulltext = hybrid(
            &server,
            with(json!({"query": "hybridfoldneedle AND beta", "mode": "fulltext"})),
        )
        .await;
        assert_eq!(
            entity_ids(&fulltext),
            vec!["transcript:codex:fold-session:1:0"],
            "{fulltext}"
        );

        let reader = coordinates["exact_read"]["arguments"].clone();
        assert_eq!(coordinates["exact_read"]["tool"], "bbox_context");
        let page = server
            .bbox_context(Parameters(
                serde_json::from_value::<ContextToolParams>(reader).unwrap(),
            ))
            .await;
        assert_ne!(page.is_error, Some(true), "{page:?}");
        let page = serde_json::to_value(page).unwrap();
        let page: Value =
            serde_json::from_str(page["content"][0]["text"].as_str().unwrap()).unwrap();
        let body: Value = serde_json::from_str(page["body"]["text"].as_str().unwrap()).unwrap();
        assert_eq!(body["content"], "hybridfoldneedle alpha");
    }

    /// Tool invocations are searchable through their transcript records alone:
    /// each event is one hit with readable conversation coordinates, and the
    /// retired tool-call document type matches nothing.
    #[tokio::test]
    async fn hybrid_tool_use_search_reads_transcript_records_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(&root),
        ));
        {
            let index = server.state.idx.read();
            let fields = index.field_handles();
            let mut writer = index
                .index_handle()
                .writer::<tantivy::TantivyDocument>(15_000_000)
                .unwrap();
            for (offset, role, content) in [
                (0u64, "user", "run toolneedle checks"),
                (
                    1,
                    "tool_use",
                    "tool:Bash {\"command\":\"toolneedle --check\"}",
                ),
                (
                    2,
                    "tool_use",
                    "tool:Read {\"file_path\":\"/repo/toolneedle.rs\"}",
                ),
            ] {
                let mut doc = tantivy::TantivyDocument::new();
                doc.add_text(fields.doc_type, "transcript");
                doc.add_text(fields.content, content);
                doc.add_text(fields.file_path, "native:synthetic/tool-stream");
                doc.add_text(fields.session_id, "tool-session");
                doc.add_text(fields.account, "claude");
                doc.add_text(fields.source, "claude");
                doc.add_text(fields.project, "/synthetic/tool-project");
                doc.add_text(fields.timestamp, "2026-09-01T00:00:00Z");
                doc.add_text(fields.role, role);
                doc.add_u64(fields.is_subagent, 0);
                doc.add_u64(fields.byte_offset, offset);
                writer.add_document(doc).unwrap();
            }
            writer.commit().unwrap();
            index.reader_reload_for_test();
        }
        let current = server.state.code_read_view.read().clone();
        *server.state.code_read_view.write() =
            std::sync::Arc::new(crate::server::state::CodeReadView {
                active_selectors: current.active_selectors.clone(),
                searcher: server.state.idx.read().searcher(),
                catalog_epoch: current.catalog_epoch,
                git_overlays: current.git_overlays.clone(),
            });

        let tool_uses = hybrid(
            &server,
            json!({"query": "toolneedle", "role": "tool_use",
                "include_vectors": false, "rerank": "none"}),
        )
        .await;
        let mut ids = entity_ids(&tool_uses);
        ids.sort();
        assert_eq!(
            ids,
            vec![
                "transcript:claude:tool-session:1:0",
                "transcript:claude:tool-session:2:0",
            ],
            "{tool_uses}"
        );
        for hit in tool_uses["results"].as_array().unwrap() {
            assert_eq!(hit["conversation"]["session_id"], "tool-session", "{hit}");
        }

        let retired = hybrid(
            &server,
            json!({"query": "toolneedle", "doc_type": "tool_call",
                "include_vectors": false, "rerank": "none"}),
        )
        .await;
        assert!(entity_ids(&retired).is_empty(), "{retired}");
    }
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::transcripts_tools()
}

/// Filter-class boundary for the corpus listing family (`bbox_sessions_list`):
/// resolve the raw selector once here and hand the index engine a typed
/// filter. The literal travels unchanged so the substring lane keeps its
/// semantics; the `base_project_id` term lane fires only when the selector
/// resolved to a registered project.
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
        name = "bbox_hybrid_search",
        description = "Search the corpus (transcripts, code, docs, commits, knowledge, threads, graph vertices) with BM25 and vectors. Filter conversations by role, account, source, author or channel; mode=fulltext takes raw Tantivy/Lucene syntax. Conversation hits carry read coordinates for bbox_context and bbox_messages. Use debug for ranking diagnostics."
    )]
    pub(crate) async fn bbox_hybrid_search(
        &self,
        Parameters(p): Parameters<HybridSearchParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking_with_structured("bbox_hybrid_search", move || {
            let mut p = p;
            p.resolved_project_id = server.resolve_hybrid_project_filter(p.project.as_deref());
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

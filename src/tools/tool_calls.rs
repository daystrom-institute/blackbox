use crate::index::{ProjectFilterInput, TranscriptIndex};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use serde_json::json;
use tantivy::query::{BooleanQuery, Occur, TermQuery};
use tantivy::schema::IndexRecordOption;
use tantivy::{Term, collector::TopDocs};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::tool_call_tools()
}

fn glob_matches(pattern: &str, s: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return s == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut cursor = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let rest = &s[cursor..];
        if i == 0 {
            if !rest.starts_with(part) {
                return false;
            }
            cursor += part.len();
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(pos) => cursor += pos + part.len(),
                None => return false,
            }
        }
    }
    true
}

// ── Helper: first stored text from a Tantivy doc ─────────────────────

fn doc_text(doc: &tantivy::TantivyDocument, field: tantivy::schema::Field) -> String {
    doc.get_first(field)
        .and_then(|v| match v {
            tantivy::schema::OwnedValue::Str(s) => Some(s.as_str()),
            _ => None,
        })
        .unwrap_or("")
        .to_string()
}

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolCallsParams {
    /// Filter by MCP server name (e.g. "blackbox"). Empty = all servers.
    #[serde(default)]
    pub server: Option<String>,
    /// Exact tool name filter.
    #[serde(default)]
    pub tool_name: Option<String>,
    /// Glob pattern matched against tool names within the selected server scope.
    /// Supports `*` wildcard (e.g. `bbox_*`). Applied within each candidate page.
    #[serde(default)]
    pub glob_pattern: Option<String>,
    /// Filter by tool kind: read | write | edit | bash | mcp | unknown
    #[serde(default)]
    pub tool_kind: Option<String>,
    /// Substring match on tool_target (file path, cwd, repo).
    #[serde(default)]
    pub tool_target: Option<String>,
    /// Filter by project path (cwd recorded at call time).
    #[serde(default)]
    pub project: Option<String>,
    /// ISO 8601 lower bound on timestamp (lexical compare).
    #[serde(default)]
    pub since: Option<String>,
    /// Max rows to return (default 20, max 100).
    #[serde(default)]
    pub limit: Option<u32>,
    /// Continue with next_offset from the previous response, using identical filters.
    /// This is a candidate cursor; a filtered page can be empty and still have more.
    #[serde(default)]
    pub offset: Option<usize>,
}

fn query_tool_calls(
    idx: &TranscriptIndex,
    p: &ToolCallsParams,
    project_filter: Option<&ProjectFilterInput>,
) -> anyhow::Result<String> {
    let searcher = idx.searcher();
    let fields = idx.field_handles();

    let mut clauses: Vec<(Occur, Box<dyn tantivy::query::Query>)> = vec![(
        Occur::Must,
        Box::new(TermQuery::new(
            Term::from_field_text(fields.doc_type, "tool_call"),
            IndexRecordOption::Basic,
        )),
    )];

    if let Some(s) = p.server.as_deref().filter(|s| !s.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_server, s),
                IndexRecordOption::Basic,
            )),
        ));
    }

    if let Some(n) = p.tool_name.as_deref().filter(|n| !n.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_name, n),
                IndexRecordOption::Basic,
            )),
        ));
    }

    if let Some(k) = p.tool_kind.as_deref().filter(|k| !k.is_empty()) {
        clauses.push((
            Occur::Must,
            Box::new(TermQuery::new(
                Term::from_field_text(fields.tool_kind, k),
                IndexRecordOption::Basic,
            )),
        ));
    }

    let limit = p.limit.unwrap_or(20).clamp(1, 100) as usize;
    let offset = p.offset.unwrap_or(0);
    anyhow::ensure!(
        offset <= 100_000,
        "offset exceeds 100000; narrow the filters"
    );
    let query = BooleanQuery::new(clauses);
    // Scan a bounded candidate page. Continuation counts consumed candidates,
    // so selective post-filters cannot silently hide later matching calls.
    let candidates = searcher.search(&query, &TopDocs::with_limit(1001).and_offset(offset))?;
    let candidate_count = candidates.len();
    let project_filter = project_filter
        .cloned()
        .or_else(|| p.project.as_deref().map(ProjectFilterInput::unresolved));
    let mut rows = Vec::new();
    let mut consumed = 0usize;
    let mut bytes = 0usize;
    for (_, addr) in candidates.into_iter().take(1000) {
        let doc: tantivy::TantivyDocument = searcher.doc(addr)?;
        let name = doc_text(&doc, fields.tool_name);
        let target = doc_text(&doc, fields.tool_target);
        let project = doc_text(&doc, fields.project);
        let base_project = doc_text(&doc, fields.base_project_id);
        let timestamp = doc_text(&doc, fields.timestamp);
        if p.glob_pattern
            .as_ref()
            .is_some_and(|glob| !glob_matches(glob, &name))
            || p.tool_target
                .as_ref()
                .is_some_and(|wanted| !target.contains(wanted))
            || p.since
                .as_ref()
                .is_some_and(|since| timestamp.is_empty() || timestamp < *since)
            || project_filter.as_ref().is_some_and(|filter| {
                !project.contains(&filter.literal)
                    && !filter
                        .project_id
                        .as_ref()
                        .is_some_and(|id| id == &base_project)
            })
        {
            consumed += 1;
            continue;
        }
        let mut row = json!({
            "server": doc_text(&doc, fields.tool_server),
            "tool_name": name,
            "tool_kind": doc_text(&doc, fields.tool_kind),
            "target": target,
            "outcome": doc_text(&doc, fields.tool_outcome),
            "session_id": doc_text(&doc, fields.session_id),
            "project": project,
            "timestamp": timestamp,
            "task_id": doc_text(&doc, fields.task_id),
        });
        row.as_object_mut()
            .unwrap()
            .retain(|_, v| v.as_str() != Some(""));
        let row_bytes = serde_json::to_vec(&row)?.len();
        if bytes + row_bytes > 32_000 {
            anyhow::ensure!(
                !rows.is_empty(),
                "tool-call record exceeds the response budget; narrow the query to other records"
            );
            break;
        }
        bytes += row_bytes;
        consumed += 1;
        rows.push(row);
        if rows.len() == limit {
            break;
        }
    }
    let next_offset = (consumed < candidate_count).then_some(offset + consumed);
    Ok(serde_json::to_string(
        &json!({"rows": rows, "next_offset": next_offset}),
    )?)
}

#[tool_router(router = tool_call_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_tool_calls",
        description = "Search indexed tool-call history by server, tool name, kind, target, project and time. Returns bounded rows and next_offset; paths in records describe historical calls, not files the caller must open."
    )]
    pub(crate) async fn bbox_tool_calls(
        &self,
        Parameters(p): Parameters<ToolCallsParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_tool_calls", move || {
            let filter =
                crate::tools::transcripts::corpus_project_filter(&server, p.project.as_deref());
            query_tool_calls(&server.state.idx.read(), &p, filter.as_ref())
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_history_continues_past_empty_filtered_pages_and_preserves_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let idx = TranscriptIndex::open_or_create_with_records(
            &root.join("idx"),
            Vec::new(),
            None,
            root.join("projects.json"),
            root.join("kb.json"),
            root.join("threads.json"),
            root.join("roadmap.json"),
            std::sync::Arc::new(crate::index::StaticProjectRecordsProvider::empty()),
        )
        .unwrap();
        let fields = idx.field_handles();
        let mut writer = idx
            .index_handle()
            .writer::<tantivy::TantivyDocument>(15_000_000)
            .unwrap();
        for i in 0..1005 {
            let mut doc = tantivy::TantivyDocument::new();
            doc.add_text(fields.doc_type, "tool_call");
            doc.add_text(fields.tool_server, "fixture-server");
            doc.add_text(fields.tool_name, "fixture-tool");
            doc.add_text(fields.tool_target, if i < 1000 { "skip" } else { "wanted" });
            doc.add_text(fields.session_id, format!("session-{i}"));
            writer.add_document(doc).unwrap();
        }
        writer.commit().unwrap();
        idx.reader_reload_for_test();
        let mut params = ToolCallsParams {
            server: Some("fixture-server".into()),
            tool_name: Some("fixture-tool".into()),
            tool_target: Some("wanted".into()),
            limit: Some(2),
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        let mut pages = 0;
        loop {
            let page: serde_json::Value =
                serde_json::from_str(&query_tool_calls(&idx, &params, None).unwrap()).unwrap();
            let rows = page["rows"].as_array().unwrap();
            assert!(rows.len() <= 2);
            for row in rows {
                assert_eq!(row["server"], "fixture-server");
                assert_eq!(row["tool_name"], "fixture-tool");
                assert_eq!(row["target"], "wanted");
                assert!(seen.insert(row["session_id"].as_str().unwrap().to_string()));
            }
            pages += 1;
            assert!(pages < 10, "cursor must progress");
            let Some(next) = page["next_offset"].as_u64() else {
                break;
            };
            assert!(next as usize > params.offset.unwrap_or(0));
            params.offset = Some(next as usize);
        }
        assert_eq!(seen.len(), 5);
        params.server = Some("different-server".into());
        params.offset = None;
        let page: serde_json::Value =
            serde_json::from_str(&query_tool_calls(&idx, &params, None).unwrap()).unwrap();
        assert!(page["rows"].as_array().unwrap().is_empty());
        assert!(page["next_offset"].is_null());
    }
}

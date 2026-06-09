use std::collections::BTreeMap;

use crate::knowledge::{
    DecideParams, ForgetParams, KnowledgeLinkParams, KnowledgeListParams, LearnParams,
    RememberParams, ResponseFormat,
};
use crate::packets::packet_matches_query;
use crate::server::BlackboxServer;
use crate::system_memory;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::json;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::knowledge_tools()
}

fn has_runtime_knowledge_filter(p: &KnowledgeListParams) -> bool {
    p.scope.is_some()
        || p.project.is_some()
        || p.provider.is_some()
        || p.status.is_some()
        || p.approval.is_some()
}

/// Extract the top knowledge entry id from a `kb.list` entries block for the
/// response breadcrumb. The block opens with `N entries:\n\n[<id>] …`, so the
/// first bracketed token is the highest-ranked entry. Returns None for the
/// "No entries found." sentinel (no `[`), so a packet-only or memory-only
/// response does not emit a spurious entry pointer.
fn first_entry_id(entries_block: &str) -> Option<String> {
    let start = entries_block.find('[')? + 1;
    let end = entries_block[start..].find(']')? + start;
    let id = entries_block[start..end].trim();
    (!id.is_empty()).then(|| id.to_string())
}

fn entry_ids(entries_block: &str) -> Vec<String> {
    entries_block
        .lines()
        .filter_map(|line| {
            let rest = line.strip_prefix('[')?;
            let end = rest.find(']')?;
            let id = rest[..end].trim();
            (!id.is_empty()).then(|| id.to_string())
        })
        .collect()
}

fn log_tool_ok(tool: &'static str, start: std::time::Instant, bytes: usize) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes, "ok");
}

fn log_tool_err(tool: &'static str, start: std::time::Instant, err: &anyhow::Error) {
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %err, "err");
}

fn matches_system_memory_catalog(category: Option<&str>) -> bool {
    matches!(
        category,
        Some("system_memory") | Some("system-memory") | Some("system_memories")
    )
}

fn format_system_memory_catalog(query: Option<&str>) -> String {
    system_memory::format_catalog_summary(query)
}

fn exact_system_memory_response(p: &KnowledgeListParams) -> Option<String> {
    if has_runtime_knowledge_filter(p) {
        return None;
    }
    if matches_system_memory_catalog(p.category.as_deref()) {
        return Some(format_system_memory_catalog(p.query.as_deref()));
    }
    if p.category.is_some() {
        return None;
    }
    let memory = system_memory::exact_query(p.query.as_deref())?;
    let mut out = String::new();
    out.push_str("── System memories ──────────────────────────\n");
    out.push_str(&system_memory::format_for_listing(memory));
    Some(out)
}

#[tool_router(router = knowledge_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_learn",
        description = "Persist a user-stated rule or convention that should bind future sessions; rendered into provider markdown files. Use for narrative rules (\"we always X\", \"never Y\"). If the rule you're storing is actually a priority-ordered decision function, classification rubric, or structured mechanism — use `bbox_compile` instead; that produces a shareable packet any agent can apply deterministically."
    )]
    pub(crate) async fn bbox_learn(
        &self,
        Parameters(p): Parameters<LearnParams>,
    ) -> CallToolResult {
        let format = match ResponseFormat::parse_optional(p.format.as_deref()) {
            Ok(format) => format,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let warning = self.arc_bound_warning(p.id.as_deref(), &p.content);
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut kb = server.state.kb.write();
            let result = kb.learn_result(&p, false)?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_learn", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                match format {
                    ResponseFormat::Text => {
                        let mut text = match warning {
                            Some(w) => format!("{}{}", result.message, w),
                            None => result.message,
                        };
                        if let Some(rider) = &rider {
                            text.push_str(rider);
                        }
                        log_tool_ok("bbox_learn", start, text.len());
                        Self::ok_text(&text)
                    }
                    ResponseFormat::Json => {
                        let message = match &rider {
                            Some(rider) => format!("{}{}", result.message, rider),
                            None => result.message,
                        };
                        let mut payload = serde_json::json!({
                            "id": result.id,
                            "action": result.action,
                            "rendered": result.rendered,
                            "render_pending": result.render_pending,
                            "message": message,
                        });
                        if let Some(summary) = result.summary {
                            payload["summary"] = serde_json::json!(summary);
                        }
                        if let Some(w) = warning {
                            payload["warnings"] = serde_json::json!([w.trim().to_string()]);
                        }
                        let bytes = serde_json::to_string(&payload)
                            .map(|s| s.len())
                            .unwrap_or_default();
                        log_tool_ok("bbox_learn", start, bytes);
                        Self::ok_json(&payload)
                    }
                }
            }
            Err(e) => {
                log_tool_err("bbox_learn", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    pub(crate) async fn bbox_remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut kb = server.state.kb.write();
            let result = kb.remember_result(&p, false)?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_remember", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_remember", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_remember", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    pub(crate) async fn bbox_decide(
        &self,
        Parameters(p): Parameters<DecideParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let mut kb = server.state.kb.write();
            let result = kb.decide_result(&p, false)?;
            let rider = kb.repo_record_rider(&result.id);
            Ok::<_, anyhow::Error>((result, rider))
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge write task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok((result, rider)) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_decide", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                    tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
                }
                if let Some(old_id) = result.superseded.as_deref() {
                    if let Err(err) = self.tombstone_knowledge_entry_in_index(old_id) {
                        tracing::warn!(error = %err, entry = %old_id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                    }
                }
                let mut message = result.message;
                if let Some(rider) = rider {
                    message.push_str(&rider);
                }
                log_tool_ok("bbox_decide", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_decide", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces matching rule-packets and system memories; system memories include system_memory:<id> refs usable with bbox_inspect_entity or bbox_bundle_evidence. Pass category=\"packet\" to list compiled packets, category=\"system_memory\" to list memory metadata, or bbox_packet_list for structured packet filters."
    )]
    pub(crate) async fn bbox_knowledge(
        &self,
        Parameters(p): Parameters<KnowledgeListParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_knowledge", move || {
            if let Some(out) = exact_system_memory_response(&p) {
                return Ok(out);
            }

            let mut combined = server.state.kb.write().list(&p)?;
            // Captured before packets/memories are appended, so it reflects the
            // top knowledge entry (not a packet/memory line).
            let top_entry_id = first_entry_id(&combined);
            let recall_ids = entry_ids(&combined);
            if !recall_ids.is_empty() {
                let recall_result = {
                    let mut kb = server.state.kb.write();
                    kb.record_recall(&recall_ids)
                };
                match recall_result {
                    Ok(()) => {
                        // Recall telemetry was always best-effort on this read path;
                        // keep it write-behind rather than making queries wait for fsync.
                        server.state.kb_persister.request();
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "knowledge recall telemetry update failed");
                    }
                }
            }

            // Surface matching packets. Uses the same match semantics as
            // bbox_packet_list so the two tools agree on what "matches" means.
            let all_packets = server.state.packets.read().list_all()?;
            let matching_packets: Vec<_> =
                if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                    all_packets
                        .into_iter()
                        .filter(|pkt| packet_matches_query(pkt, q))
                        .collect()
                } else if p.category.as_deref() == Some("packet") {
                    all_packets
                } else {
                    Vec::new()
                };

            if !matching_packets.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Rule-packets ───────────────────────────────\n");
                let limit = p.limit.unwrap_or(50).min(500) as usize;
                for pkt in matching_packets.iter().take(limit) {
                    let histogram: Vec<String> = pkt
                        .rules
                        .iter()
                        .fold(BTreeMap::<String, usize>::new(), |mut acc, r| {
                            *acc.entry(r.classification.clone()).or_insert(0) += 1;
                            acc
                        })
                        .into_iter()
                        .map(|(k, v)| format!("{k}:{v}"))
                        .collect();
                    combined.push_str(&format!(
                        "[{}] Packet | domain: {} | scope: {} | {} rules [{}] | created {}\n",
                        pkt.id,
                        pkt.domain,
                        pkt.scope,
                        pkt.rules.len(),
                        histogram.join(", "),
                        pkt.created_at,
                    ));
                }
                combined.push_str(
                    "  (use bbox_packet_list for filter/query/preview; bbox_apply to evaluate)\n",
                );
            }

            // Also surface matching system memories. See
            // system-defaults/memories/ — these are file-loaded runbooks
            // read at startup, queryable but never rendered.
            //
            // The broad query path renders compact signposts, not full bodies:
            // a fuzzy multi-term query matches many runbooks, and full bodies
            // (~40KB each) overflow the token budget. The agent pulls a full
            // body via the exact-id short-circuit (`bbox_knowledge(query="sm-…")`,
            // handled above by exact_system_memory_response).
            let memories = system_memory::search(p.query.as_deref());
            if !memories.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                for m in memories {
                    combined.push_str(&system_memory::format_for_signpost(m));
                    combined.push('\n');
                }
                combined.push_str(
                    "  (signposts only — query an exact sm-* id for the full runbook body)\n",
                );
            }

            // Top-level breadcrumb: pull the highest-ranked knowledge entry into
            // the graph funnel. Packets and memories carry their own pointers
            // above; this completes the response-breadcrumb plane for entries.
            if let Some(id) = &top_entry_id {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── Next steps ───────────────────────────────\n");
                combined.push_str(&format!(
                    "  → Inspect the top entry's edges + provenance: bbox_inspect_entity(entity_ref=\"knowledge:{id}\")\n"
                ));
                combined.push_str(&format!(
                    "  → Package an answer: bbox_bundle_evidence(question=<q>, entity_refs=[\"knowledge:{id}\"])\n"
                ));
            }
            Ok(combined)
        })
        .await
    }

    #[tool(name = "bbox_knowledge_link", description = "Append a knowledge edge.")]
    pub(crate) async fn bbox_knowledge_link(
        &self,
        Parameters(p): Parameters<KnowledgeLinkParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || {
            let edge = server.state.kb.write().append_link(&p)?;
            Ok::<_, anyhow::Error>(serde_json::to_string_pretty(&json!({
                "status": "linked",
                "source": p.source,
                "target": p.target,
                "kind": edge.kind.edge_kind(),
                "confidence": edge.confidence,
            }))?)
        })
        .await
        .map_err(|e| anyhow::anyhow!("knowledge link task failed: {e}"))
        .and_then(std::convert::identity);

        match write_result {
            Ok(text) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_knowledge_link", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                log_tool_ok("bbox_knowledge_link", start, text.len());
                Self::ok_text(&text)
            }
            Err(e) => {
                log_tool_err("bbox_knowledge_link", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    pub(crate) async fn bbox_forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
    ) -> CallToolResult {
        let start = std::time::Instant::now();
        let id = p.id.clone();
        let server = self.clone();
        let write_result = tokio::task::spawn_blocking(move || server.state.kb.write().forget(&p))
            .await
            .map_err(|e| anyhow::anyhow!("knowledge forget task failed: {e}"))
            .and_then(std::convert::identity);

        match write_result {
            Ok(message) => {
                if let Err(e) = self.state.kb_persister.request_durable().await {
                    log_tool_err("bbox_forget", start, &e);
                    return Self::err_text(&format!("Error: {e:#}"));
                }
                if let Err(err) = self.tombstone_knowledge_entry_in_index(&id) {
                    tracing::warn!(error = %err, entry = %id, "knowledge index tombstone failed; will reconstruct on next reindex cycle");
                }
                log_tool_ok("bbox_forget", start, message.len());
                Self::ok_text(&message)
            }
            Err(e) => {
                log_tool_err("bbox_forget", start, &e);
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_system_memory() {
        system_memory::init_for_tests();
    }

    #[test]
    fn first_entry_id_extracts_top_and_handles_sentinel() {
        let block = "2 entries:\n\n[abc123] Convention/project | all | title\n  \
                     content_bytes=10\n  body [with brackets]\n\n[def456] ...";
        assert_eq!(first_entry_id(block).as_deref(), Some("abc123"));
        assert_eq!(first_entry_id("No entries found."), None);
        assert_eq!(first_entry_id(""), None);
    }

    #[test]
    fn exact_system_memory_response_returns_only_exact_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            query: Some("sm-refactor".into()),
            ..Default::default()
        })
        .expect("exact canonical system memory query should short-circuit");

        assert!(out.contains("[system] sm-refactor"));
        assert!(!out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[bb-tool-reference]"));
        assert!(!out.contains("No entries found."));
    }

    #[test]
    fn exact_system_memory_response_respects_runtime_filters() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            scope: Some("project".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });

        assert!(out.is_none());
    }

    #[test]
    fn system_memory_catalog_returns_all_memories() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            ..Default::default()
        })
        .expect("system_memory category should return catalog");

        assert!(out.contains("── System memories"));
        assert!(out.contains("[system] sm-rule-packets"));
        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-agentic-opening-sequence"));
        assert!(
            !out.contains("bbox_compile"),
            "catalog listing should not include full body"
        );
    }

    #[test]
    fn system_memory_catalog_accepts_hyphenated_and_plural_forms() {
        init_system_memory();
        for form in &["system_memory", "system-memory", "system_memories"] {
            let out = exact_system_memory_response(&KnowledgeListParams {
                category: Some(form.to_string()),
                ..Default::default()
            });
            assert!(out.is_some(), "category={} should match", form);
        }
    }

    #[test]
    fn system_memory_catalog_supports_query_filter() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("system_memory".into()),
            query: Some("refactor".into()),
            ..Default::default()
        })
        .expect("system_memory + query should return filtered catalog");

        assert!(out.contains("[system] sm-refactor"));
        assert!(out.contains("[system] sm-refactor-rust"));
        assert!(!out.contains("[system] sm-rule-packets"));
    }

    #[test]
    fn system_memory_category_does_not_match_memory() {
        init_system_memory();
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("memory".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });
        assert!(out.is_none());
    }
}

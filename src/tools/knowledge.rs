use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::knowledge_tools()
}

fn has_runtime_knowledge_filter(p: &KnowledgeListParams) -> bool {
    p.category.is_some()
        || p.scope.is_some()
        || p.project.is_some()
        || p.provider.is_some()
        || p.status.is_some()
        || p.approval.is_some()
}

fn exact_system_memory_response(p: &KnowledgeListParams) -> Option<String> {
    if has_runtime_knowledge_filter(p) {
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
    pub(crate) fn bbox_learn(&self, Parameters(p): Parameters<LearnParams>) -> CallToolResult {
        let format = match ResponseFormat::parse_optional(p.format.as_deref()) {
            Ok(format) => format,
            Err(e) => return Self::err_text(&format!("Error: {e:#}")),
        };
        let start = std::time::Instant::now();
        match (|| {
            let warning = self.arc_bound_warning(p.id.as_deref(), &p.content);
            let result = self.state.kb.write().learn_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            Ok::<_, anyhow::Error>((result, warning))
        })() {
            Ok((result, warning)) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                match format {
                    ResponseFormat::Text => {
                        let text = match warning {
                            Some(w) => format!("{}{}", result.message, w),
                            None => result.message,
                        };
                        tracing::info!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, bytes = text.len(), "ok");
                        Self::ok_text(&text)
                    }
                    ResponseFormat::Json => {
                        let mut payload = serde_json::json!({
                            "id": result.id,
                            "action": result.action,
                            "rendered": result.rendered,
                            "render_pending": result.render_pending,
                            "message": result.message,
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
                        tracing::info!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, bytes, "ok");
                        Self::ok_json(&payload)
                    }
                }
            }
            Err(e) => {
                let ms = start.elapsed().as_secs_f64() * 1000.0;
                tracing::warn!(target: "blackbox::tool", tool = "bbox_learn", elapsed_ms = ms, error = %e, "err");
                Self::err_text(&format!("Error: {e:#}"))
            }
        }
    }

    #[tool(
        name = "bbox_remember",
        description = "Persist a fact for later recall; indexed but NOT rendered."
    )]
    pub(crate) fn bbox_remember(
        &self,
        Parameters(p): Parameters<RememberParams>,
    ) -> CallToolResult {
        Self::run("bbox_remember", || {
            let result = self.state.kb.write().remember_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            Ok(result.message)
        })
    }

    #[tool(
        name = "bbox_decide",
        description = "Record a durable commitment with required rationale; supports supersession."
    )]
    pub(crate) fn bbox_decide(&self, Parameters(p): Parameters<DecideParams>) -> CallToolResult {
        Self::run("bbox_decide", || {
            let result = self.state.kb.write().decide_result(&p, false)?;
            if let Err(err) = self.sync_knowledge_entry_to_index(&result.id) {
                tracing::warn!(error = %err, entry = %result.id, "knowledge index sync failed; will reconstruct on next reindex cycle");
            }
            if let Some(old_id) = result.superseded.as_deref() {
                self.tombstone_knowledge_entry_in_index(old_id)?;
            }
            Ok(result.message)
        })
    }

    #[tool(
        name = "bbox_knowledge",
        description = "Query durable knowledge entries by free-text or filters. Use early when prior decisions, conventions, remembered facts, or system runbooks could change the answer. Also surfaces (a) rule-packets matching the query by id / domain / rule ids / classification values, and (b) system memories (code-embedded runbooks) marked `[system]`. Pass `category=\"packet\"` to list every compiled packet regardless of query. For structured packet discovery + filtering, use bbox_packet_list."
    )]
    pub(crate) fn bbox_knowledge(
        &self,
        Parameters(p): Parameters<KnowledgeListParams>,
    ) -> CallToolResult {
        Self::run("bbox_knowledge", || {
            if let Some(out) = exact_system_memory_response(&p) {
                return Ok(out);
            }

            let mut combined = self.state.kb.write().list(&p)?;

            // Surface matching packets. Uses the same match semantics as
            // bbox_packet_list so the two tools agree on what "matches" means.
            let all_packets = self.state.packets.read().list_all()?;
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

            // Also surface matching code-embedded memories. See
            // src/system_memory/ — these are static runbooks baked into the
            // binary via include_str!, queryable but never rendered.
            let memories = system_memory::search(p.query.as_deref());
            if !memories.is_empty() {
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str("\n── System memories ──────────────────────────\n");
                for m in memories {
                    combined.push_str(&system_memory::format_for_listing(m));
                    combined.push('\n');
                }
            }
            Ok(combined)
        })
    }

    #[tool(name = "bbox_knowledge_link", description = "Append a knowledge edge.")]
    pub(crate) fn bbox_knowledge_link(
        &self,
        Parameters(p): Parameters<KnowledgeLinkParams>,
    ) -> CallToolResult {
        Self::run("bbox_knowledge_link", || {
            let edge = self.state.kb.write().append_link(&p)?;
            Ok(serde_json::to_string_pretty(&json!({
                "status": "linked",
                "source": p.source,
                "target": p.target,
                "kind": edge.kind.edge_kind(),
                "confidence": edge.confidence,
            }))?)
        })
    }

    #[tool(name = "bbox_forget", description = "Retire or supersede an entry.")]
    pub(crate) fn bbox_forget(&self, Parameters(p): Parameters<ForgetParams>) -> CallToolResult {
        Self::run("bbox_forget", || {
            let message = self.state.kb.write().forget(&p)?;
            self.tombstone_knowledge_entry_in_index(&p.id)?;
            Ok(message)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_system_memory_response_returns_only_exact_memory() {
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
        let out = exact_system_memory_response(&KnowledgeListParams {
            category: Some("tool".into()),
            query: Some("sm-refactor".into()),
            ..Default::default()
        });

        assert!(out.is_none());
    }
}

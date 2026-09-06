use crate::packets::{
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams, packet_matches_query, packet_summary,
};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde_json::Value;

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct PacketListToolParams {
    #[serde(flatten)]
    pub filters: PacketListParams,
    /// Exact packet id, for selecting one revision.
    #[serde(default)]
    pub packet_id: Option<String>,
    /// Continue with next_offset; newest-created first, then id ascending.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Include classification histogram and rule id preview (default false).
    #[serde(default)]
    pub detail: bool,
}

fn packet_list_page(
    mut packets: Vec<crate::packets::Packet>,
    params: &PacketListToolParams,
) -> anyhow::Result<Value> {
    let p = &params.filters;
    packets.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    if let Some(id) = &params.packet_id {
        packets.retain(|packet| &packet.id == id);
    }
    if let Some(domain) = &p.domain {
        packets.retain(|packet| &packet.domain == domain);
    }
    if let Some(scope) = &p.scope {
        packets.retain(|packet| &packet.scope == scope);
    }
    if let Some(query) = p.query.as_deref().filter(|q| !q.is_empty()) {
        packets.retain(|packet| packet_matches_query(packet, query));
    }
    if p.latest_per_domain.unwrap_or(false) {
        let mut seen = std::collections::HashSet::new();
        packets.retain(|packet| seen.insert(packet.domain.clone()));
    }
    let total = packets.len();
    let limit = p.limit.unwrap_or(20).clamp(1, 100);
    let offset = params.offset.unwrap_or(0);
    let packets: Vec<_> = packets
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|packet| {
            if params.detail {
                packet_summary(&packet)
            } else {
                let mut row = serde_json::json!({"id": packet.id, "domain": packet.domain, "scope": packet.scope,
                "rules_count": packet.rules.len(), "created_at": packet.created_at});
                bbox_corpus_core::response_page::preview_field(&mut row, "domain", 200);
                row
            }
        })
        .collect();
    let next_offset = offset.saturating_add(packets.len());
    bbox_corpus_core::response_page::bound_page(
        serde_json::json!({"count": packets.len(), "packets": packets, "total": total, "offset": offset, "limit": limit,
            "next_offset": (next_offset < total).then_some(next_offset), "order": "created_at_desc,id_asc",
            "detail_hint": "Rule previews: bbox_packet_list(packet_id=<id>,detail=true). Complete JSON: bbox_inspect_entity(entity_ref=packet:<id>,property=body); continue with property_cursor=body.next_cursor.",
        }),
        "packets",
    )
}

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::packets_tools()
}

#[tool_router(router = packets_tools)]
impl BlackboxServer {
    // ── Rule-packets (compressive compilation of observations) ────────

    #[tool(
        name = "bbox_compile",
        description = "Compile a rubric / judge / decision-function into a shareable packet. Reach here when you're writing a priority-ordered rubric, ranking proposals against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones. Symptom: you're about to paste the same rubric text into multiple sub-agent prompts — compile once and dispatch the packet_id instead. Rules are first-match-wins over a predicate AST; validate with bbox_audit before trusting. Packets compose via `Apply{packet_id, expect}` — extract `is_breaking` / `privileged_role` / etc. once, reuse across packets. Full workflow: sm-rule-packets via bbox_knowledge."
    )]
    pub(crate) async fn bbox_compile(
        &self,
        Parameters(p): Parameters<CompileParams>,
    ) -> CallToolResult {
        // compile persists the packet file and appends to the event log
        // (fsync) under packets.read() — run on the blocking pool.
        let server = self.clone();
        Self::run_blocking("bbox_compile", move || {
            // Phase-2 §9.2 B7: project-scoped packets resolve their scope
            // through the shared engine at write time, stamping the durable
            // key plus the stable id for dual-read; misses keep the caller's
            // literal scope with no id, exactly like the other owner stores.
            let mut p = p;
            if let Some(raw) = p.project.clone()
                && let Some(resolution) = server.resolve_project_filter(&raw)
                && let bbox_corpus_core::project_selector::ProjectResolution::Attached(ctx) =
                    resolution
            {
                p.project = Some(ctx.store_key.clone());
                p.project_id = Some(ctx.project.project_id().to_owned());
            }
            server.state.packets.read().compile(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_apply",
        description = "Evaluate a packet against one entity — deterministic, no LLM. The receive-side of the packet workflow: a sub-agent that received packet_id from its orchestrator calls this to classify without reinterpreting the rubric. mode=\"first\" returns the first matching rule; mode=\"all\" returns every matching rule plus an aggregate verdict (for review / multi-finding shape). Cheap at arbitrary scale."
    )]
    pub(crate) async fn bbox_apply(
        &self,
        Parameters(p): Parameters<PacketApplyParams>,
    ) -> CallToolResult {
        // apply_tool loads the packet from disk and appends apply events.
        let server = self.clone();
        Self::run_blocking("bbox_apply", move || {
            server.state.packets.read().apply_tool(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_audit",
        description = "Run a packet against a {entity, expected}[] dataset; report fidelity + mismatching rule ids. The self-verify step: a packet with fidelity < 1.0 is lying about its training data. ALWAYS call this after bbox_compile against the observations you derived the rules from — catches over-generalization, rule-ordering bugs, and field-name typos."
    )]
    pub(crate) async fn bbox_audit(
        &self,
        Parameters(p): Parameters<AuditParams>,
    ) -> CallToolResult {
        // audit_tool loads the packet from disk and appends audit events.
        let server = self.clone();
        Self::run_blocking("bbox_audit", move || {
            server.state.packets.read().audit_tool(&p)
        })
        .await
    }

    #[tool(
        name = "bbox_packet_list",
        description = "Discover compiled packet summary pages (default 20, maximum 100). Filter before paging and continue with next_offset. detail=true adds histograms and rule previews. Read complete rules with bbox_inspect_entity(entity_ref=packet:<id>, property=body)."
    )]
    pub(crate) async fn bbox_packet_list(
        &self,
        Parameters(p): Parameters<PacketListToolParams>,
    ) -> CallToolResult {
        // list_all re-reads the packet store from disk.
        let server = self.clone();
        Self::run_blocking("bbox_packet_list", move || {
            let packets = server.state.packets.read().list_all()?;
            Ok(serde_json::to_string(&packet_list_page(packets, &p)?)?)
        })
        .await
    }

    #[tool(
        name = "bbox_packet_events",
        description = "Query the packet operation log — every compile / apply / audit / gap event the daemon has recorded, plus `repair_candidate` events emitted by the self-heal scanner when enabled. Use to investigate packet behavior over time: low-fidelity audits, high no_match rates, compile failures, authoring gaps, and packets the scanner has flagged for repair. Filter by op (compile / apply / audit / gap / repair_candidate), packet_id, outcome, or since. Returns newest-first up to `limit` (default 50, max 500)."
    )]
    pub(crate) async fn bbox_packet_events(
        &self,
        Parameters(p): Parameters<EventsParams>,
    ) -> CallToolResult {
        // list_events reads the event log from disk.
        let server = self.clone();
        Self::run_blocking("bbox_packet_events", move || {
            let limit = p.limit.unwrap_or(50).min(500);
            let events = server.state.packets.read().list_events(
                p.op.as_deref(),
                p.packet_id.as_deref(),
                p.outcome.as_deref(),
                p.since.as_deref(),
                limit,
            )?;
            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "count": events.len(),
                "limit": limit,
                "events": events,
            }))?)
        })
        .await
    }

    #[tool(
        name = "bbox_packet_gap",
        description = "Log a packet-authoring gap: 'I wanted to compile a rule but the AST couldn't express it'. Use when you fall back to prose, ad-hoc code, or a different tool because a primitive you needed isn't available. The `description` names what you wanted; `ast_feature_requested` names the primitive you wished existed (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`). These gaps are the highest-signal input for prioritizing new AST primitives — every gap logged is a vote for what the packet system can't yet say. Query via bbox_packet_events(op='gap')."
    )]
    pub(crate) async fn bbox_packet_gap(
        &self,
        Parameters(p): Parameters<GapParams>,
    ) -> CallToolResult {
        // log_gap appends a fsync'd packet event and the companion note
        // writes the gap store — blocking pool, not a tokio worker.
        let server = self.clone();
        match tokio::task::spawn_blocking(move || server.bbox_packet_gap_sync(p)).await {
            Ok(result) => result,
            Err(e) => Self::err_text(&format!("packet gap task failed: {e}")),
        }
    }

    fn bbox_packet_gap_sync(&self, p: GapParams) -> CallToolResult {
        let start = std::time::Instant::now();
        let tool = "bbox_packet_gap";

        let ev = {
            let guard = self.state.packets.read();
            match guard.log_gap(
                &p.description,
                p.domain.as_deref(),
                p.attempted_sketch.as_deref(),
                p.fallback_used.as_deref(),
                p.ast_feature_requested.as_deref(),
            ) {
                Ok(ev) => ev,
                Err(e) => {
                    let ms = start.elapsed().as_secs_f64() * 1000.0;
                    tracing::warn!(target: "blackbox::tool", tool, elapsed_ms = ms, error = %e, "err");
                    return Self::err_text(&format!("Error: {e:#}"));
                }
            }
        };

        let warning = crate::gaps::emit_companion_packet_gap_note(&self.state.gaps, &ev, &p);

        let mut response = serde_json::json!({
            "logged": true,
            "timestamp": ev.timestamp,
            "note": "Thank you — this gap is now queryable via bbox_packet_events(op='gap')",
        });
        if let Some(w) = warning {
            response["companion_note_warning"] = Value::String(w);
        }

        let text = serde_json::to_string_pretty(&response).unwrap_or_default();
        let ms = start.elapsed().as_secs_f64() * 1000.0;
        tracing::info!(target: "blackbox::tool", tool, elapsed_ms = ms, bytes = text.len(), "ok");
        Self::ok_text(&text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn packet_rules_are_recoverable_through_exact_property_pages() {
        use crate::mcp_tools::inspect::InspectEntityParams;
        use crate::server::state::SharedState;
        use std::sync::Arc;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let params: CompileParams = serde_json::from_value(json!({
            "domain":"inspection-fixture", "scope":"global",
            "rank_table":{"operator":7}, "threshold_table":{"record":3},
            "rank_lookup_key":"actor", "threshold_lookup_key":"asset",
            "classification_lattice":["pass"], "prefix_inference":{"allow_":"pass"},
            "rules":[{"id":"allow_complete", "antecedent":{"op":"True"},
                "consequent":{"message":"bounded \"λ🙂\" ".repeat(700)}, "classification":"pass"}],
            "source_ids":["fixture:packet-inspection"],
        }))
        .unwrap();
        server.state.packets.read().compile(&params).unwrap();
        let packet = server.state.packets.read().list_all().unwrap().remove(0);
        let entity_ref = format!("packet:{}", packet.id);
        let expected = serde_json::to_value(&packet).unwrap();
        let overview = server
            .bbox_inspect_entity(Parameters(
                serde_json::from_value::<InspectEntityParams>(
                    json!({"entity_ref":entity_ref, "property_mode":"summary", "per_type_limit":0}),
                )
                .unwrap(),
            ))
            .await;
        assert!(!overview.is_error.unwrap_or(false), "{overview:?}");
        let overview: Value =
            serde_json::from_str(&overview.content[0].as_text().unwrap().text).unwrap();
        assert!(overview["properties"].get("body").is_none());
        assert!(
            overview["property_projection"]["omitted_keys"]
                .as_array()
                .unwrap()
                .contains(&json!("body"))
        );

        let mut cursor: Option<String> = None;
        let mut body = String::new();
        let mut pages = 0;
        loop {
            let response = server
                .bbox_inspect_entity(Parameters(
                    serde_json::from_value::<InspectEntityParams>(
                        json!({"entity_ref":entity_ref, "property":"body", "property_cursor":cursor,
                        "property_limit":1023, "per_type_limit":0}),
                    )
                    .unwrap(),
                ))
                .await;
            assert!(!response.is_error.unwrap_or(false), "{response:?}");
            assert!(serde_json::to_vec(&response).unwrap().len() < 8192);
            let page: Value =
                serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(page["body"]["offset"], body.len());
            let fragment = page["body"]["text"].as_str().unwrap();
            assert!(!fragment.is_empty() && fragment.len() <= 1023);
            body.push_str(fragment);
            pages += 1;
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                assert_eq!(page["body"]["total_bytes"], body.len());
                break;
            }
        }
        assert!(pages > 2);
        assert_eq!(serde_json::from_str::<Value>(&body).unwrap(), expected);
    }

    #[test]
    fn packet_summary_pages_filter_before_paging_and_expand_histograms() {
        let packets: Vec<crate::packets::Packet> = (0..105).rev().map(|i| serde_json::from_value(json!({
            "id": format!("packet-{i:08x}"), "domain": format!("domain-{}", i % 2), "scope": "global",
            "rules": [], "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z",
        })).unwrap()).collect();
        let mut p: PacketListToolParams = serde_json::from_value(json!({"limit": 1000})).unwrap();
        let first = packet_list_page(packets.clone(), &p).unwrap();
        assert_eq!(first["packets"].as_array().unwrap().len(), 100);
        assert_eq!(first["next_offset"], 100);
        assert_eq!(first["packets"][0]["id"], "packet-00000000");
        assert!(
            first["packets"][0]
                .get("classification_histogram")
                .is_none()
        );
        p.offset = Some(100);
        let last = packet_list_page(packets.clone(), &p).unwrap();
        assert_eq!(last["packets"].as_array().unwrap().len(), 5);
        assert!(last["next_offset"].is_null());
        p.offset = Some(0);
        p.filters.domain = Some("domain-1".into());
        let filtered = packet_list_page(packets.clone(), &p).unwrap();
        assert_eq!(filtered["total"], 52);
        p.packet_id = Some("packet-00000001".into());
        p.detail = true;
        let detail = packet_list_page(packets, &p).unwrap();
        assert_eq!(detail["total"], 1);
        assert!(
            detail["packets"][0]
                .get("classification_histogram")
                .is_some()
        );
    }
}

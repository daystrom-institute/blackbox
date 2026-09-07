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
pub(crate) struct PacketApplyToolParams {
    #[serde(flatten)]
    pub packet: PacketApplyParams,
    /// Continue mode=all findings in packet rule order.
    #[serde(default)]
    pub finding_offset: Option<usize>,
    /// Maximum mode=all findings per page (default 100, maximum 100).
    #[serde(default)]
    pub finding_limit: Option<usize>,
    /// Expand small finding consequents; oversized values use exact result pages.
    #[serde(default)]
    pub finding_detail: bool,
    /// Exact complete result, without adding another observation event. Re-evaluates
    /// the same input; changed input or result refuses continuation.
    #[serde(default)]
    pub result_cursor: Option<String>,
    /// Select exact JSON result pages (default/max 4096 bytes).
    #[serde(default)]
    pub result_body_limit: Option<usize>,
}

#[derive(Debug, serde::Deserialize, rmcp::schemars::JsonSchema)]
pub(crate) struct PacketAuditToolParams {
    #[serde(flatten)]
    pub packet: AuditParams,
    /// Continue mismatch pages in dataset order.
    #[serde(default)]
    pub mismatch_offset: Option<usize>,
    /// Maximum mismatches per page (default 100, maximum 100).
    #[serde(default)]
    pub mismatch_limit: Option<usize>,
    /// Expand small mismatch values; oversized values use exact result pages.
    #[serde(default)]
    pub mismatch_detail: bool,
    /// Exact complete result, without adding another observation event. Re-evaluates
    /// the same input; changed input or result refuses continuation.
    #[serde(default)]
    pub result_cursor: Option<String>,
    /// Select exact JSON result pages (default/max 4096 bytes).
    #[serde(default)]
    pub result_body_limit: Option<usize>,
}

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
            "pagination": "live_offset: installs and removals can move rows; restart at offset 0 after changing packets",
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
        description = "Compile a rubric / judge / decision-function into a shareable packet. Reach here when you're writing a priority-ordered rubric, ranking proposals against shared criteria, compressing an access table, coordinating sub-agents against identical standards, or classifying future cases the same way you classified past ones. Symptom: you're about to paste the same rubric text into multiple sub-agent prompts - compile once and dispatch the packet_id instead. Rules are first-match-wins over a predicate AST; validate with bbox_audit before trusting. Packets compose via `Apply{packet_id, expect}` - extract `is_breaking` / `privileged_role` / etc. once, reuse across packets. Full workflow: sm-rule-packets via bbox_knowledge."
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
        description = "Evaluate a packet against one entity deterministically, without an LLM. mode=\"first\" returns the first matching rule; mode=\"all\" returns one bounded finding page plus an aggregate verdict. Continue finding pages with next_finding_offset."
    )]
    pub(crate) async fn bbox_apply(
        &self,
        Parameters(p): Parameters<PacketApplyToolParams>,
    ) -> CallToolResult {
        // apply_tool loads the packet from disk and appends apply events.
        let server = self.clone();
        Self::run_blocking("bbox_apply", move || {
            if p.result_cursor.is_some() || p.result_body_limit.is_some() {
                anyhow::ensure!(p.finding_offset.is_none() && p.finding_limit.is_none() && !p.finding_detail,
                    "exact result pages use result_cursor/result_body_limit; omit row paging and detail");
                return server.state.packets.read().apply_result_body(
                    &p.packet, p.result_cursor.as_deref(), p.result_body_limit);
            }
            server.state.packets.read().apply_tool_paged(
                &p.packet,
                p.finding_offset.unwrap_or(0),
                p.finding_limit
                    .unwrap_or(crate::packets::MAX_PACKET_RESULT_ROWS),
                p.finding_detail,
            )
        })
        .await
    }

    #[tool(
        name = "bbox_audit",
        description = "Run a packet against a mode-specific {entity, expectation}[] dataset and report fidelity plus bounded mismatch pages. Fidelity measures agreement with the supplied dataset, not universal classifier correctness. Continue mismatches with next_mismatch_offset."
    )]
    pub(crate) async fn bbox_audit(
        &self,
        Parameters(p): Parameters<PacketAuditToolParams>,
    ) -> CallToolResult {
        // audit_tool loads the packet from disk and appends audit events.
        let server = self.clone();
        Self::run_blocking("bbox_audit", move || {
            if p.result_cursor.is_some() || p.result_body_limit.is_some() {
                anyhow::ensure!(p.mismatch_offset.is_none() && p.mismatch_limit.is_none() && !p.mismatch_detail,
                    "exact result pages use result_cursor/result_body_limit; omit row paging and detail");
                return server.state.packets.read().audit_result_body(
                    &p.packet, p.result_cursor.as_deref(), p.result_body_limit);
            }
            server.state.packets.read().audit_tool_paged(
                &p.packet,
                p.mismatch_offset.unwrap_or(0),
                p.mismatch_limit
                    .unwrap_or(crate::packets::MAX_PACKET_RESULT_ROWS),
                p.mismatch_detail,
            )
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
        description = "Query bounded pages of the live packet operation log. Returns newest-first rows with total, explicit ordering, next_cursor, and live-view continuation semantics. Filter by closed op/outcome enums, packet_id, or RFC 3339 since."
    )]
    pub(crate) async fn bbox_packet_events(
        &self,
        Parameters(p): Parameters<EventsParams>,
    ) -> CallToolResult {
        // list_events reads the event log from disk.
        let server = self.clone();
        Self::run_blocking("bbox_packet_events", move || {
            let page = server.state.packets.read().events_page(&p)?;
            Ok(serde_json::to_string(&page)?)
        })
        .await
    }

    #[tool(
        name = "bbox_packet_gap",
        description = "Log a packet-authoring gap: 'I wanted to compile a rule but the AST couldn't express it'. Use when you fall back to prose, ad-hoc code, or a different tool because a primitive you needed isn't available. The `description` names what you wanted; `ast_feature_requested` names the primitive you wished existed (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`). These gaps are the highest-signal input for prioritizing new AST primitives - every gap logged is a vote for what the packet system can't yet say. Query via bbox_packet_events(op='gap')."
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
    async fn large_first_consequent_and_audit_values_have_exact_result_pages() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let server = BlackboxServer::new(std::sync::Arc::new(
            crate::server::state::SharedState::for_test(&root),
        ));
        let expected = "large \"界\n".repeat(2500);
        let params: CompileParams = serde_json::from_value(json!({"domain":"result-fixture", "scope":"global",
            "rules":[{"id":"match", "classification":"pass", "antecedent":{"op":"True"}, "consequent":expected}]})).unwrap();
        server.state.packets.read().compile(&params).unwrap();
        let id = server.state.packets.read().list_all().unwrap().remove(0).id;
        let ordinary = server
            .bbox_apply(Parameters(
                serde_json::from_value(json!({"packet_id":id, "entity":{}})).unwrap(),
            ))
            .await;
        assert_ne!(ordinary.is_error, Some(true));
        assert!(serde_json::to_vec(&ordinary).unwrap().len() < 4096);
        let mut cursor = None;
        let mut text = String::new();
        loop {
            let response = server
                .bbox_apply(Parameters(
                    serde_json::from_value(json!({"packet_id":id, "entity":{},
                "result_body_limit":1024, "result_cursor":cursor}))
                    .unwrap(),
                ))
                .await;
            assert_ne!(response.is_error, Some(true));
            assert!(serde_json::to_vec(&response).unwrap().len() < 8192);
            let page: Value =
                serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(page["observation_event"], "not_requested");
            text.push_str(page["body"]["text"].as_str().unwrap());
            cursor = page["body"]["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        let result: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(result["prediction"]["consequent"], expected);
        let audit = server.bbox_audit(Parameters(serde_json::from_value(json!({"packet_id":id,
            "dataset":[{"entity":{"large":expected},"expected":"different"}], "mismatch_detail":true})).unwrap())).await;
        assert_ne!(audit.is_error, Some(true));
        assert!(serde_json::to_vec(&audit).unwrap().len() < 8192);
        let args = json!({"packet_id":id,"dataset":[{"entity":{},"expected":"different"}],"result_body_limit":512});
        let first = server
            .bbox_audit(Parameters(serde_json::from_value(args.clone()).unwrap()))
            .await;
        let page: Value = serde_json::from_str(&first.content[0].as_text().unwrap().text).unwrap();
        let mut changed = args;
        changed["result_cursor"] = page["body"]["next_cursor"].clone();
        assert!(changed["result_cursor"].is_string());
        changed["dataset"][0]["entity"] = json!({"changed":true});
        let stale = server
            .bbox_audit(Parameters(serde_json::from_value(changed).unwrap()))
            .await;
        assert_eq!(stale.is_error, Some(true));
    }

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
                "consequent":"bounded \"λ🙂\" ".repeat(700), "classification":"pass"}],
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
    fn packet_test_server(rules: Value) -> (tempfile::TempDir, BlackboxServer, String) {
        use crate::server::state::SharedState;
        use std::sync::Arc;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let params: CompileParams = serde_json::from_value(json!({
            "domain": "packet-page-fixture",
            "scope": "global",
            "classification_lattice": ["flag", "pass"],
            "rules": rules,
        }))
        .unwrap();
        let compiled = server.state.packets.read().compile(&params).unwrap();
        let packet_id = compiled.split_whitespace().nth(1).unwrap().to_owned();
        (temporary, server, packet_id)
    }

    fn numbered_rules(count: usize) -> Value {
        json!(
            (0..count)
                .map(|index| {
                    json!({
                        "id": format!("r{index:03}"),
                        "classification": if index == 0 { "flag" } else { "pass" },
                        "antecedent": {"op": "True"},
                        "consequent": format!("FINDING_{index}"),
                    })
                })
                .collect::<Vec<_>>()
        )
    }
    #[test]
    fn packet_event_operations_use_a_closed_enum() {
        let error = serde_json::from_value::<EventsParams>(json!({
            "op": "audit-invalid-operation"
        }))
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unknown variant"), "{message}");
        assert!(message.contains("audit-invalid-operation"), "{message}");

        let error = serde_json::from_value::<EventsParams>(json!({
            "outcome": "invalid-outcome"
        }))
        .unwrap_err();
        let message = error.to_string();
        assert!(message.contains("unknown variant"), "{message}");
        assert!(message.contains("invalid-outcome"), "{message}");

        serde_json::from_value::<EventsParams>(json!({
            "op": "gc",
            "outcome": "partial"
        }))
        .unwrap();
    }
    #[test]
    fn oversized_packet_compile_is_rejected_before_logging_an_event() {
        use crate::server::state::SharedState;
        use std::sync::Arc;

        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let server = BlackboxServer::new(Arc::new(SharedState::for_test(&root)));
        let rules = numbered_rules(crate::packets::MAX_PACKET_RULES + 1);
        let params: CompileParams = serde_json::from_value(json!({
            "domain": "oversized-compile",
            "scope": "global",
            "classification_lattice": ["flag", "pass"],
            "rules": rules,
        }))
        .unwrap();
        let error = server.state.packets.read().compile(&params).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("at most 500"), "{message}");
        let compiles = server
            .state
            .packets
            .read()
            .list_events(Some("compile"), None, None, None, 500)
            .unwrap();
        assert!(
            compiles.is_empty(),
            "oversized compile must not write an event"
        );
    }

    #[tokio::test]
    async fn packet_apply_all_pages_findings_and_discloses_observation_writes() {
        let (_temporary, server, packet_id) = packet_test_server(numbered_rules(3));
        let first = server
            .bbox_apply(Parameters(
                serde_json::from_value::<PacketApplyToolParams>(json!({
                    "packet_id": packet_id,
                    "entity": {"case": "fixture"},
                    "mode": "all",
                    "finding_offset": 0,
                    "finding_limit": 1,
                }))
                .unwrap(),
            ))
            .await;
        assert!(!first.is_error.unwrap_or(false), "{first:?}");
        let first: Value = serde_json::from_str(&first.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(first["finding_count"], 3);
        assert_eq!(first["findings"].as_array().unwrap().len(), 1);
        assert_eq!(first["findings"][0]["rule_id"], "r000");
        assert!(first["findings"][0].get("consequent").is_none());
        assert!(first["findings"][0].get("consequent_preview").is_some());
        let exact_reader = first["exact_reader"].as_str().unwrap();
        assert!(exact_reader.starts_with("bbox_inspect_entity"));

        let second = server
            .bbox_apply(Parameters(
                serde_json::from_value::<PacketApplyToolParams>(json!({
                    "packet_id": packet_id,
                    "entity": {"case": "fixture"},
                    "mode": "all",
                    "finding_offset": 2,
                    "finding_limit": 1,
                    "finding_detail": true,
                }))
                .unwrap(),
            ))
            .await;
        assert!(!second.is_error.unwrap_or(false), "{second:?}");
        let second: Value =
            serde_json::from_str(&second.content[0].as_text().unwrap().text).unwrap();
        let second_row = &second["findings"][0];
        assert_eq!(second_row["rule_id"], "r002");
        assert_eq!(second_row["consequent"], "FINDING_2");
        assert_eq!(second["next_finding_offset"], serde_json::Value::Null);
    }
    #[tokio::test]
    async fn packet_audit_pages_mismatches_and_rejects_oversized_batches_before_events() {
        let (_temporary, server, packet_id) = packet_test_server(json!([{
            "id": "reject",
            "classification": "flag",
            "antecedent": {"op": "True"},
            "consequent": "REJECT",
        }]));
        let dataset = json!([
            {"entity": {"case": 0}, "expected": "WRONG"},
            {"entity": {"case": 1}, "expected": "WRONG"},
            {"entity": {"case": 2}, "expected": "WRONG"}
        ]);
        let mut offset = 0;
        let mut indexes = Vec::new();
        loop {
            let response = server
                .bbox_audit(Parameters(
                    serde_json::from_value::<PacketAuditToolParams>(json!({
                        "packet_id": packet_id,
                        "mode": "first",
                        "mismatch_offset": offset,
                        "mismatch_limit": 1,
                        "dataset": dataset,
                    }))
                    .unwrap(),
                ))
                .await;
            assert!(!response.is_error.unwrap_or(false), "{response:?}");
            let page: Value =
                serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
            assert_eq!(page["total"], 3);
            assert_eq!(page["mismatch_count"], 3);
            assert_eq!(page["fidelity"], 0.0);
            assert_eq!(page["observation_event"], "written");
            indexes.push(page["mismatches"][0]["dataset_index"].as_u64().unwrap());
            offset = match page["next_mismatch_offset"].as_u64() {
                Some(next) => usize::try_from(next).unwrap(),
                None => break,
            };
        }
        assert_eq!(indexes, vec![0, 1, 2]);

        let oversized_dataset: Vec<Value> = (0..=crate::packets::MAX_AUDIT_DATASET_ROWS)
            .map(|index| json!({"entity": {"case": index}, "expected": "REJECT"}))
            .collect();
        let rejected = server
            .bbox_audit(Parameters(
                serde_json::from_value::<PacketAuditToolParams>(json!({
                    "packet_id": packet_id,
                    "mode": "first",
                    "dataset": oversized_dataset,
                }))
                .unwrap(),
            ))
            .await;
        assert!(rejected.is_error.unwrap_or(false), "{rejected:?}");
        let error = &rejected.content[0].as_text().unwrap().text;
        assert!(error.contains("at most 1000"), "{error}");

        let audits = server
            .state
            .packets
            .read()
            .list_events(Some("audit"), None, None, None, 500)
            .unwrap();
        assert_eq!(audits.len(), 3, "oversized batch must not write an event");
    }
    #[tokio::test]
    async fn packet_audit_all_preserves_mode_specific_item_outcomes() {
        let (_temporary, server, packet_id) = packet_test_server(numbered_rules(3));
        let response = server
            .bbox_audit(Parameters(
                serde_json::from_value::<PacketAuditToolParams>(json!({
                    "packet_id": packet_id,
                    "mode": "all",
                    "mismatch_limit": 1,
                    "dataset": [
                        {
                            "entity": {"case": "complete"},
                            "expected_rule_ids": ["r000", "r001", "r002"]
                        },
                        {
                            "entity": {"case": "incomplete"},
                            "expected_rule_ids": ["r000"]
                        }
                    ],
                }))
                .unwrap(),
            ))
            .await;
        assert!(!response.is_error.unwrap_or(false), "{response:?}");
        let page: Value =
            serde_json::from_str(&response.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(page["total"], 2);
        assert_eq!(page["correct"], 1);
        assert_eq!(page["mismatch_count"], 1);
        let mismatch = &page["mismatches"][0];
        assert_eq!(mismatch["dataset_index"], 1);
        assert_eq!(mismatch["check"], "rule_ids");
        assert_eq!(mismatch["expected_rule_ids"], json!(["r000"]));
        assert_eq!(mismatch["actual_rule_ids"], json!(["r000", "r001", "r002"]));
        assert_eq!(page["next_mismatch_offset"], serde_json::Value::Null);
    }
    #[test]
    fn large_apply_all_finding_pages_recover_every_item_in_order() {
        let (_temporary, server, packet_id) = packet_test_server(numbered_rules(250));
        let packets = server.state.packets.read();
        let params = PacketApplyParams {
            packet_id: packet_id.clone(),
            entity: json!({"case": "large"}),
            mode: Some(crate::packets::ApplyMode::All),
        };
        let mut offset = 0;
        let mut rule_ids = Vec::new();
        while offset < 250 {
            let response = packets
                .apply_tool_paged(&params, offset, 100, false)
                .unwrap();
            let page: Value = serde_json::from_str(&response).unwrap();
            assert!(serde_json::to_vec(&page).unwrap().len() <= 24 * 1024);
            let rows = page["findings"].as_array().unwrap();
            assert!(!rows.is_empty());
            rule_ids.extend(
                rows.iter()
                    .map(|row| row["rule_id"].as_str().unwrap().to_owned()),
            );
            match page["next_finding_offset"].as_u64() {
                Some(next) => offset = next as usize,
                None => break,
            }
        }
        assert_eq!(rule_ids.len(), 250);
        assert_eq!(rule_ids[0], "r000");
        assert_eq!(rule_ids[249], "r249");
    }
}

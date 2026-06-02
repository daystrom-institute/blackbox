use crate::packets::{
    ApplyParams as PacketApplyParams, AuditParams, CompileParams, EventsParams, GapParams,
    PacketListParams, Packets, packet_matches_query, packet_summary,
};
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};
use serde_json::Value;

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
    pub(crate) fn bbox_compile(&self, Parameters(p): Parameters<CompileParams>) -> CallToolResult {
        Self::run("bbox_compile", || self.state.packets.read().compile(&p))
    }

    #[tool(
        name = "bbox_apply",
        description = "Evaluate a packet against one entity — deterministic, no LLM. The receive-side of the packet workflow: a sub-agent that received packet_id from its orchestrator calls this to classify without reinterpreting the rubric. mode=\"first\" returns the first matching rule; mode=\"all\" returns every matching rule plus an aggregate verdict (for review / multi-finding shape). Cheap at arbitrary scale."
    )]
    pub(crate) fn bbox_apply(
        &self,
        Parameters(p): Parameters<PacketApplyParams>,
    ) -> CallToolResult {
        Self::run("bbox_apply", || self.state.packets.read().apply_tool(&p))
    }

    #[tool(
        name = "bbox_audit",
        description = "Run a packet against a {entity, expected}[] dataset; report fidelity + mismatching rule ids. The self-verify step: a packet with fidelity < 1.0 is lying about its training data. ALWAYS call this after bbox_compile against the observations you derived the rules from — catches over-generalization, rule-ordering bugs, and field-name typos."
    )]
    pub(crate) fn bbox_audit(&self, Parameters(p): Parameters<AuditParams>) -> CallToolResult {
        Self::run("bbox_audit", || self.state.packets.read().audit_tool(&p))
    }

    #[tool(
        name = "bbox_packet_list",
        description = "Discover compiled rule-packets before authoring a new one. Filter by `domain` (exact), `scope` (global/project), or `query` (case-insensitive substring across id, domain, rule ids, classification values). Pass `latest_per_domain=true` to collapse multiple revisions of the same domain. Each summary includes a classification histogram and the first few rule ids so you can judge relevance without calling bbox_apply. If a packet already covers your domain, compose it via `Apply{packet_id, expect}` or reuse via `bbox_apply`. See sm-rule-packets via bbox_knowledge."
    )]
    pub(crate) fn bbox_packet_list(
        &self,
        Parameters(p): Parameters<PacketListParams>,
    ) -> CallToolResult {
        Self::run("bbox_packet_list", || {
            let mut packets = self.state.packets.read().list_all()?;
            if let Some(domain) = &p.domain {
                packets.retain(|pkt| pkt.domain == *domain);
            }
            if let Some(scope) = &p.scope {
                packets.retain(|pkt| &pkt.scope == scope);
            }
            if let Some(q) = p.query.as_deref().filter(|q| !q.is_empty()) {
                packets.retain(|pkt| packet_matches_query(pkt, q));
            }
            if p.latest_per_domain.unwrap_or(false) {
                // list_all returns newest-first; keep the first occurrence of each domain.
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                packets.retain(|pkt| seen.insert(pkt.domain.clone()));
            }
            let limit = p.limit.unwrap_or(50).min(500);
            packets.truncate(limit);

            let summaries: Vec<_> = packets.iter().map(packet_summary).collect();

            Ok(serde_json::to_string_pretty(&serde_json::json!({
                "count": summaries.len(),
                "limit": limit,
                "packets": summaries,
            }))?)
        })
    }

    #[tool(
        name = "bbox_packet_events",
        description = "Query the packet operation log — every compile / apply / audit / gap event the daemon has recorded, plus `repair_candidate` events emitted by the self-heal scanner when enabled. Use to investigate packet behavior over time: low-fidelity audits, high no_match rates, compile failures, authoring gaps, and packets the scanner has flagged for repair. Filter by op (compile / apply / audit / gap / repair_candidate), packet_id, outcome, or since. Returns newest-first up to `limit` (default 50, max 500)."
    )]
    pub(crate) fn bbox_packet_events(
        &self,
        Parameters(p): Parameters<EventsParams>,
    ) -> CallToolResult {
        Self::run("bbox_packet_events", || {
            let limit = p.limit.unwrap_or(50).min(500);
            let events = self.state.packets.read().list_events(
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
    }

    #[tool(
        name = "bbox_packet_gap",
        description = "Log a packet-authoring gap: 'I wanted to compile a rule but the AST couldn't express it'. Use when you fall back to prose, ad-hoc code, or a different tool because a primitive you needed isn't available. The `description` names what you wanted; `ast_feature_requested` names the primitive you wished existed (e.g. `RateCmp`, `StringMatches`, `Within{temporal}`). These gaps are the highest-signal input for prioritizing new AST primitives — every gap logged is a vote for what the packet system can't yet say. Query via bbox_packet_events(op='gap')."
    )]
    pub(crate) fn bbox_packet_gap(&self, Parameters(p): Parameters<GapParams>) -> CallToolResult {
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

        let warning = Packets::emit_companion_gap_note(&self.state.gaps, &ev, &p);

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

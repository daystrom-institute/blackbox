use crate::packets;
use crate::server::BlackboxServer;
use crate::server::surface;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::schemars;
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::mcp_surface_tools()
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpSurfaceAction {
    Replay,
    List,
    Describe,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum McpSurfaceDetail {
    Policy,
}

#[derive(Debug, Deserialize, Serialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct McpSurfaceParams {
    /// Action to perform. "replay" evaluates a surface; "list" pages the
    /// installed surface-packet catalog; "describe" pages packet rule
    /// summaries for a selected surface.
    pub action: McpSurfaceAction,
    /// Surface name for replay/describe. Defaults to "default".
    #[serde(default)]
    pub surface: Option<String>,
    /// Project selector (registered ID or alias) for replay/describe policy
    /// selection. List is global and rejects this selector.
    #[serde(default)]
    pub project: Option<String>,
    /// Exact allow/disallow policy detail for replay/describe. The default
    /// projection reports counts; complete packet JSON remains available
    /// through packet body pages.
    #[serde(default)]
    pub detail: Option<McpSurfaceDetail>,
    /// Maximum rows per page (default 20, maximum 100).
    #[serde(default)]
    pub limit: Option<usize>,
    /// Continue with next_offset. Inventories are live views, not snapshots.
    #[serde(default)]
    pub offset: Option<usize>,
    /// Exact projected response JSON, 4..=4096 bytes, including all rows. Omit limit/offset;
    /// packet body readers remain the source for complete original rule JSON.
    #[serde(default)]
    pub body_limit: Option<usize>,
    /// Continue body.next_cursor with identical action/surface/project/detail.
    /// Changed routing evidence or selection refuses continuation.
    #[serde(default)]
    pub cursor: Option<String>,
}
impl McpSurfaceParams {
    fn validate(&self) -> anyhow::Result<()> {
        let exact = self.cursor.is_some() || self.body_limit.is_some();
        anyhow::ensure!(
            !exact || (self.limit.is_none() && self.offset.is_none()),
            "exact body reads use cursor/body_limit; omit limit and offset"
        );
        anyhow::ensure!(
            self.body_limit
                .is_none_or(|limit| (4..=4096).contains(&limit)),
            "body_limit must be between 4 and 4096"
        );
        if let Some(cursor) = self.cursor.as_deref() {
            let valid = cursor.split_once(':').is_some_and(|(hash, offset)| {
                hash.len() == 64
                    && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    && offset.parse::<usize>().is_ok()
            });
            anyhow::ensure!(valid, "invalid cursor; use body.next_cursor");
        }
        anyhow::ensure!(
            !matches!(self.action, McpSurfaceAction::List)
                || (self.surface.is_none() && self.project.is_none() && self.detail.is_none()),
            "action=list accepts limit/offset or cursor/body_limit; surface, project, and detail are replay/describe selectors"
        );
        Ok(())
    }
}

#[tool_router(router = mcp_surface_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_mcp_surface",
        description = "Inspect MCP routing: replay pages visible tools, describe pages rules, list pages surface packets. detail=policy expands allow/disallow patterns. body_limit/cursor without limit/offset recovers complete projected JSON; changed policy or selection refuses continuation."
    )]
    pub(crate) async fn bbox_mcp_surface(
        &self,
        Parameters(p): Parameters<McpSurfaceParams>,
    ) -> CallToolResult {
        let server = self.clone();
        Self::run_blocking("bbox_mcp_surface", move || {
            // Filter-class engine resolution (phase-2 §9.2 B8), aligned
            // with the `/mcp?project=` wire head: a resolving selector
            // rewrites to the packet scope key (store key, or the stable
            // project id for an attachment-less catalog identity); a miss
            // keeps the literal. The resolved id additionally feeds the
            // packet store's dual-read arm.
            p.validate()?;
            let mut p = p;
            let mut resolved_project_id = None;
            if let Some(raw) = p.project.clone() {
                p.project = Some(match server.resolve_project_filter(&raw) {
                    Some(resolution) => {
                        resolved_project_id = resolution.project_id().map(str::to_owned);
                        match resolution
                            .store_key()
                            .or(resolution.project_id())
                            .map(str::to_owned)
                        {
                            Some(resolved) => resolved,
                            None => {
                                server.state.resolver_compat.record(
                                    "bbox_mcp_surface",
                                    crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                                );
                                raw
                            }
                        }
                    }
                    None => {
                        server.state.resolver_compat.record(
                            "bbox_mcp_surface",
                            crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                        );
                        raw
                    }
                });
            }
            let resolved_project_id = resolved_project_id.as_deref();
            match p.action {
                McpSurfaceAction::Replay => server.handle_mcp_surface_replay(&p, resolved_project_id),
                McpSurfaceAction::List => server.handle_mcp_surface_list(
                    p.limit.unwrap_or(20).clamp(1, 100),
                    p.offset.unwrap_or(0),
                    Some(&p),
                ),
                McpSurfaceAction::Describe => server.handle_mcp_surface_describe(&p, resolved_project_id),
            }
        })
        .await
    }
}

impl BlackboxServer {
    fn surface_policy_rows(verdict: &surface::ToolSurfaceVerdict) -> Vec<serde_json::Value> {
        match verdict {
            surface::ToolSurfaceVerdict::ToolSurface {
                allow, disallow, ..
            } => allow
                .iter()
                .map(|pattern| serde_json::json!({"kind": "allow", "pattern": pattern}))
                .chain(
                    disallow
                        .iter()
                        .map(|pattern| serde_json::json!({"kind": "disallow", "pattern": pattern})),
                )
                .collect(),
            surface::ToolSurfaceVerdict::Deny { .. } => Vec::new(),
        }
    }

    fn surface_verdict_summary(verdict: &surface::ToolSurfaceVerdict) -> serde_json::Value {
        match verdict {
            surface::ToolSurfaceVerdict::ToolSurface {
                allow,
                disallow,
                instructions,
            } => {
                let mut summary = serde_json::json!({
                    "route": "tool_surface",
                    "allow_count": allow.len(),
                    "disallow_count": disallow.len(),
                });
                if let Some(instructions) = instructions {
                    summary["instructions_preview"] =
                        serde_json::Value::String(instructions.clone());
                    bbox_corpus_core::response_page::preview_field(
                        &mut summary,
                        "instructions_preview",
                        200,
                    );
                }
                summary
            }
            surface::ToolSurfaceVerdict::Deny { reason } => {
                let mut summary = serde_json::json!({
                    "route": "deny",
                    "reason": reason.clone().unwrap_or_default(),
                });
                bbox_corpus_core::response_page::preview_field(&mut summary, "reason", 200);
                summary
            }
        }
    }

    fn surface_page(
        &self,
        mut page: serde_json::Value,
        field: &str,
        rows: Vec<serde_json::Value>,
        offset: usize,
        limit: usize,
        params: Option<&McpSurfaceParams>,
    ) -> anyhow::Result<String> {
        if let Some(p) = params {
            p.validate()?;
            if p.cursor.is_some() || p.body_limit.is_some() {
                page[field] = serde_json::json!(rows);
                page["total"] = serde_json::json!(rows.len());
                page["count"] = serde_json::json!(rows.len());
                let scope = serde_json::json!([
                    "mcp_surface",
                    p.action,
                    p.surface,
                    p.project,
                    p.detail,
                    field
                ])
                .to_string();
                let body = bbox_corpus_core::response_page::json_body_page(
                    &scope,
                    &page,
                    p.cursor.as_deref(),
                    p.body_limit,
                )?;
                return Ok(serde_json::json!({
                    "body": body,
                    "continuation": "Repeat action/surface/project/detail with cursor=body.next_cursor; concatenate body.text as JSON. Changed routing evidence refuses continuation."
                }).to_string());
            }
        }
        if offset > rows.len() {
            anyhow::bail!(
                "error.stale_surface_offset: inventory has {} rows, requested offset {offset}",
                rows.len()
            );
        }
        let total = rows.len();
        let selected: Vec<_> = rows.into_iter().skip(offset).take(limit).collect();
        let next_offset = offset.saturating_add(selected.len());
        let returned = selected.len();
        page[field] = serde_json::Value::Array(selected);
        page["count"] = serde_json::Value::from(returned);
        page["total"] = serde_json::Value::from(total);
        page["offset"] = serde_json::Value::from(offset);
        page["limit"] = serde_json::Value::from(limit);
        page["next_offset"] = if next_offset < total {
            serde_json::Value::from(next_offset)
        } else {
            serde_json::Value::Null
        };
        page["continuation_semantics"] = serde_json::Value::String(
            "live_offset: packet writes can change rows between pages; restart from offset 0 after a write".to_string(),
        );
        let mut bounded = bbox_corpus_core::response_page::bound_page(page, field)?;
        let bounded_next_offset = bounded["next_offset"]
            .as_u64()
            .unwrap_or(next_offset as u64);
        bounded["next_offset"] = if bounded_next_offset < total as u64 {
            serde_json::Value::from(bounded_next_offset)
        } else {
            serde_json::Value::Null
        };
        Ok(serde_json::to_string(&bounded)?)
    }

    fn handle_mcp_surface_replay(
        &self,
        p: &McpSurfaceParams,
        resolved_project_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let surface = p.surface.as_deref().unwrap_or("default");
        let entity = surface::build_surface_entity(surface, p.project.as_deref());

        let packets = self.state.packets.read();
        let packet_id = packets
            .load_latest_by_domain(
                surface::SURFACE_ROUTING_DOMAIN,
                p.project.as_deref(),
                resolved_project_id,
            )
            .ok()
            .flatten()
            .map(|packet| packet.id);
        let decision = surface::evaluate_tool_surface(
            &packets,
            entity.clone(),
            p.project.as_deref(),
            resolved_project_id,
        );
        drop(packets);

        let limit = p.limit.unwrap_or(20).clamp(1, 100);
        let offset = p.offset.unwrap_or(0);
        let verdict = Self::surface_verdict_summary(&decision.verdict);
        if matches!(p.detail, Some(McpSurfaceDetail::Policy)) {
            let rows = Self::surface_policy_rows(&decision.verdict);
            let page = serde_json::json!({
                "entity": entity,
                "detail": "policy",
                "policy_order": "allow_patterns_then_disallow_patterns",
                "verdict": verdict,
                "packet_id": packet_id,
                "exact_reader": packet_id.as_deref().map_or_else(
                    || "unavailable: no routing packet was selected".to_string(),
                    |id| format!("bbox_inspect_entity(entity_ref=packet:{id}, property=body)"),
                ),
            });
            return self.surface_page(page, "policy", rows, offset, limit, Some(p));
        }

        let tool_universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let rows: Vec<_> = self
            .tool_router
            .list_all()
            .iter()
            .filter(|tool| surface::tool_visible(&tool.name, &decision, &tool_universe))
            .map(|tool| serde_json::json!({"name": tool.name.to_string()}))
            .collect();
        let page = serde_json::json!({
            "entity": entity,
            "verdict_classification": if decision.is_deny() { "deny" } else { "tool_surface" },
            "verdict": verdict,
            "visible_tool_count": rows.len(),
            "tool_order": "router_registration_order",
            "policy_detail_hint": "detail=policy pages exact allow/disallow patterns",
        });
        self.surface_page(page, "visible_tools", rows, offset, limit, Some(p))
    }

    fn handle_mcp_surface_list(
        &self,
        limit: usize,
        offset: usize,
        params: Option<&McpSurfaceParams>,
    ) -> anyhow::Result<String> {
        let packets = self.state.packets.read();
        let mut all = packets.list_all()?;
        all.retain(|packet| packet.domain == surface::SURFACE_ROUTING_DOMAIN);
        all.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let rows: Vec<_> = all
            .into_iter()
            .map(|packet| {
                serde_json::json!({
                    "id": packet.id,
                    "scope": packet.scope,
                    "project": packet.project,
                    "created_at": packet.created_at,
                    "rule_count": packet.rules.len(),
                })
            })
            .collect();
        drop(packets);
        let page = serde_json::json!({
            "order": "created_at_desc,id_asc",
            "exact_reader": "bbox_inspect_entity(entity_ref=packet:<id>, property=body)",
        });
        self.surface_page(page, "surface_packets", rows, offset, limit, params)
    }

    fn handle_mcp_surface_describe(
        &self,
        p: &McpSurfaceParams,
        resolved_project_id: Option<&str>,
    ) -> anyhow::Result<String> {
        let packets = self.state.packets.read();
        let loaded = packets.load_latest_by_domain(
            surface::SURFACE_ROUTING_DOMAIN,
            p.project.as_deref(),
            resolved_project_id,
        );
        let packet = loaded?.ok_or_else(|| {
            anyhow::anyhow!(
                "no mcp-surface/routing packet found{}",
                p.project
                    .as_deref()
                    .map(|pr| format!(" for project {pr}"))
                    .unwrap_or_default()
            )
        })?;
        let packet_id = packet.id.clone();
        let packet_meta = serde_json::json!({
            "id": packet.id,
            "domain": packet.domain,
            "scope": packet.scope,
            "project": packet.project,
            "rule_count": packet.rules.len(),
        });

        let selected = p.surface.as_deref().unwrap_or("default");
        let entity = surface::build_surface_entity(selected, p.project.as_deref());
        let decision = surface::evaluate_tool_surface(
            &packets,
            entity,
            p.project.as_deref(),
            resolved_project_id,
        );
        drop(packets);

        let verdict = Self::surface_verdict_summary(&decision.verdict);
        let limit = p.limit.unwrap_or(20).clamp(1, 100);
        let offset = p.offset.unwrap_or(0);
        if matches!(p.detail, Some(McpSurfaceDetail::Policy)) {
            let rows = Self::surface_policy_rows(&decision.verdict);
            let page = serde_json::json!({
                "packet": packet_meta,
                "selected_surface": selected,
                "detail": "policy",
                "policy_order": "allow_patterns_then_disallow_patterns",
                "verdict_for_selected": verdict,
                "exact_reader": format!("bbox_inspect_entity(entity_ref=packet:{packet_id}, property=body)"),
            });
            return self.surface_page(page, "policy", rows, offset, limit, Some(p));
        }

        let rows: Vec<_> = packet
            .rules
            .iter()
            .map(|rule| {
                let (matching_surface, match_kind) = match &rule.antecedent {
                    packets::Predicate::Eq {
                        field,
                        value: packets::Value::String(value),
                    } if field == "surface" => (Some(value.as_str()), "exact_surface"),
                    packets::Predicate::True => (Some("*"), "unconditional"),
                    _ => (None, "requires_predicate_evaluation"),
                };
                serde_json::json!({
                    "id": rule.id,
                    "classification": rule.classification,
                    "matches_surface": matching_surface,
                    "surface_match_kind": match_kind,
                })
            })
            .collect();
        let page = serde_json::json!({
            "packet": packet_meta,
            "selected_surface": selected,
            "rule_order": "packet_rule_order",
            "verdict_for_selected": verdict,
            "exact_reader": format!("bbox_inspect_entity(entity_ref=packet:{packet_id}, property=body)"),
            "policy_detail_hint": "detail=policy pages exact allow/disallow patterns",
        });
        self.surface_page(page, "rules", rows, offset, limit, Some(p))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::packets::CompileParams;

    fn compile_surface_packet(
        packets: &crate::packets::Packets,
        rules: Vec<serde_json::Value>,
        scope: &str,
        project: Option<&str>,
    ) -> String {
        packets
            .compile(&CompileParams {
                domain: surface::SURFACE_ROUTING_DOMAIN.to_string(),
                rules: serde_json::Value::Array(rules),
                classification_lattice: Some(vec!["tool_surface".to_string(), "deny".to_string()]),
                prefix_inference: Some(Default::default()),
                scope: Some(scope.to_string()),
                project: project.map(|s| s.to_string()),
                project_id: None,
                source_ids: None,
                rank_lookup_key: None,
                rank_table: None,
                threshold_lookup_key: None,
                threshold_table: None,
            })
            .unwrap()
    }

    fn surface_rule(
        id: &str,
        surface_value: &str,
        allow: &[&str],
        disallow: &[&str],
        classification: &str,
    ) -> serde_json::Value {
        let consequent = if classification == "deny" {
            serde_json::json!({"route": "deny", "reason": "unknown MCP surface"})
        } else {
            serde_json::json!({"route": "tool_surface", "allow": allow, "disallow": disallow})
        };
        serde_json::json!({
            "id": id,
            "antecedent": {"op": "Eq", "field": "surface", "value": surface_value},
            "consequent": serde_json::to_string(&consequent).unwrap(),
            "classification": classification,
        })
    }

    fn catchall_deny() -> serde_json::Value {
        let c = serde_json::json!({"route": "deny", "reason": "unknown MCP surface"});
        serde_json::json!({
            "id": "deny_unknown",
            "antecedent": {"op": "True"},
            "consequent": serde_json::to_string(&c).unwrap(),
            "classification": "deny",
        })
    }

    fn make_server() -> (tempfile::TempDir, BlackboxServer) {
        let tmp = tempfile::TempDir::new().unwrap();
        let state = crate::server::state::SharedState::for_test(tmp.path());
        let server = BlackboxServer::new(std::sync::Arc::new(state));
        (tmp, server)
    }

    #[tokio::test]
    async fn exact_policy_body_recovers_oversized_patterns_and_refuses_stale_selection() {
        let (_tmp, server) = make_server();
        let pattern = format!("bbox_{}", "界\n\"".repeat(10000));
        compile_surface_packet(
            &server.state.packets.read(),
            vec![surface_rule(
                "huge",
                "readonly",
                &[&pattern],
                &[],
                "tool_surface",
            )],
            "global",
            None,
        );
        let mut args = serde_json::json!({
            "action":"replay", "surface":"readonly", "detail":"policy", "body_limit":4096
        });
        let mut recovered = String::new();
        let mut first_cursor = None;
        loop {
            let result = server
                .bbox_mcp_surface(Parameters(serde_json::from_value(args.clone()).unwrap()))
                .await;
            assert_ne!(result.is_error, Some(true), "{result:?}");
            assert!(serde_json::to_vec(&result).unwrap().len() < 24 * 1024);
            let page: serde_json::Value =
                serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
            recovered.push_str(page["body"]["text"].as_str().unwrap());
            let Some(next) = page["body"]["next_cursor"].as_str() else {
                break;
            };
            first_cursor.get_or_insert_with(|| next.to_owned());
            args["cursor"] = serde_json::json!(next);
        }
        let recovered: serde_json::Value = serde_json::from_str(&recovered).unwrap();
        assert_eq!(recovered["policy"][0]["pattern"], pattern);
        assert_eq!(recovered["total"], 1);
        args["cursor"] = serde_json::json!(first_cursor.unwrap());
        args["action"] = serde_json::json!("describe");
        let stale_selection = server
            .bbox_mcp_surface(Parameters(serde_json::from_value(args.clone()).unwrap()))
            .await;
        assert_eq!(stale_selection.is_error, Some(true));
        args["action"] = serde_json::json!("replay");
        compile_surface_packet(
            &server.state.packets.read(),
            vec![surface_rule(
                "changed",
                "readonly",
                &["bbox_search"],
                &[],
                "tool_surface",
            )],
            "global",
            None,
        );
        let stale_evidence = server
            .bbox_mcp_surface(Parameters(serde_json::from_value(args).unwrap()))
            .await;
        assert_eq!(stale_evidence.is_error, Some(true));
    }

    #[test]
    fn complex_surface_predicate_is_not_reported_as_unconditional() {
        let (_tmp, server) = make_server();
        let mut rule = surface_rule(
            "conditional",
            "readonly",
            &["bbox_search"],
            &[],
            "tool_surface",
        );
        rule["antecedent"] = serde_json::json!({"op":"All", "args":[
            {"op":"Eq", "field":"surface", "value":"readonly"},
            {"op":"True"}
        ]});
        compile_surface_packet(&server.state.packets.read(), vec![rule], "global", None);
        let params = serde_json::from_value(serde_json::json!({
            "action":"describe", "surface":"readonly"
        }))
        .unwrap();
        let output = server.handle_mcp_surface_describe(&params, None).unwrap();
        let value: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert!(value["rules"][0]["matches_surface"].is_null());
        assert_eq!(
            value["rules"][0]["surface_match_kind"],
            "requires_predicate_evaluation"
        );
        assert_eq!(value["verdict_for_selected"]["route"], "tool_surface");
        assert_eq!(value["verdict_for_selected"]["allow_count"], 1);
    }

    #[tokio::test]
    async fn exact_surface_selectors_refuse_before_packet_lookup() {
        let (_tmp, server) = make_server();
        for args in [
            serde_json::json!({"action":"describe", "body_limit":4096, "offset":0}),
            serde_json::json!({"action":"describe", "body_limit":0}),
            serde_json::json!({"action":"describe", "cursor":"malformed"}),
            serde_json::json!({"action":"list", "body_limit":4096, "project":"synthetic"}),
        ] {
            let result = server
                .bbox_mcp_surface(Parameters(serde_json::from_value(args).unwrap()))
                .await;
            assert_eq!(result.is_error, Some(true));
            assert!(
                !result.content[0]
                    .as_text()
                    .unwrap()
                    .text
                    .contains("no mcp-surface/routing packet")
            );
        }
    }

    #[tokio::test]
    async fn test_replay_readonly_returns_filtered_tools() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();

        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["bbox_search", "bbox_stats"],
                    &[],
                    "tool_surface",
                ),
                catchall_deny(),
            ],
            "global",
            None,
        );
        drop(packets);

        let result = server
            .bbox_mcp_surface(Parameters(McpSurfaceParams {
                action: McpSurfaceAction::Replay,
                surface: Some("readonly".to_string()),
                project: None,
                detail: None,
                limit: None,
                offset: None,
                body_limit: None,
                cursor: None,
            }))
            .await;
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
        let parsed: serde_json::Value =
            serde_json::from_str(&result.content[0].as_text().unwrap().text).unwrap();
        assert_eq!(parsed["verdict_classification"], "tool_surface");
        assert_eq!(parsed["verdict"]["allow_count"], 2);
        assert_eq!(parsed["verdict"]["disallow_count"], 0);
        assert!(parsed.get("policy").is_none());
        let visible: Vec<&str> = parsed["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(visible.contains(&"bbox_search"));
        assert!(visible.contains(&"bbox_stats"));
        assert!(!visible.contains(&"bbox_forget"));
    }

    #[test]
    fn test_replay_unknown_surface_returns_deny() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();

        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["bbox_search"],
                    &[],
                    "tool_surface",
                ),
                catchall_deny(),
            ],
            "global",
            None,
        );
        drop(packets);

        let result = server
            .handle_mcp_surface_replay(
                &McpSurfaceParams {
                    action: McpSurfaceAction::Replay,
                    surface: Some("unknown".to_string()),
                    project: None,
                    detail: None,
                    limit: None,
                    offset: None,
                    body_limit: None,
                    cursor: None,
                },
                None,
            )
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["verdict_classification"], "deny");
    }

    #[test]
    fn test_replay_with_project_uses_project_scoped_packet() {
        let (_tmp, server) = make_server();
        let project_path = "/home/user/test-repo";
        let packets = server.state.packets.read();

        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "default_global",
                "default",
                &["bbox_search"],
                &[],
                "tool_surface",
            )],
            "global",
            None,
        );

        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "default_project",
                "default",
                &["bbox_search", "bbox_stats"],
                &[],
                "tool_surface",
            )],
            "project",
            Some(project_path),
        );
        drop(packets);

        let result = server
            .handle_mcp_surface_replay(
                &McpSurfaceParams {
                    action: McpSurfaceAction::Replay,
                    surface: Some("default".to_string()),
                    project: Some(project_path.to_string()),
                    detail: None,
                    limit: None,
                    offset: None,
                    body_limit: None,
                    cursor: None,
                },
                None,
            )
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["verdict_classification"], "tool_surface");
        let visible: Vec<&str> = parsed["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(
            visible.contains(&"bbox_stats"),
            "project-scoped packet should allow bbox_stats: {:?}",
            visible
        );

        let result_global = server
            .handle_mcp_surface_replay(
                &McpSurfaceParams {
                    action: McpSurfaceAction::Replay,
                    surface: Some("default".to_string()),
                    project: None,
                    detail: None,
                    limit: None,
                    offset: None,
                    body_limit: None,
                    cursor: None,
                },
                None,
            )
            .unwrap();

        let parsed_global: serde_json::Value = serde_json::from_str(&result_global).unwrap();
        let visible_global: Vec<&str> = parsed_global["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap())
            .collect();
        assert!(
            !visible_global.contains(&"bbox_stats"),
            "global packet should not allow bbox_stats: {:?}",
            visible_global
        );
    }

    #[test]
    fn test_list_returns_surface_packets() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();

        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["bbox_search"],
                    &[],
                    "tool_surface",
                ),
                catchall_deny(),
            ],
            "global",
            None,
        );
        drop(packets);

        let result = server.handle_mcp_surface_list(20, 0, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 1);
        assert_eq!(parsed["total"], 1);
        assert_eq!(parsed["offset"], 0);
        assert_eq!(parsed["limit"], 20);
        assert_eq!(parsed["next_offset"], serde_json::Value::Null);
        assert!(
            parsed["continuation_semantics"]
                .as_str()
                .unwrap()
                .starts_with("live_offset")
        );
        let arr = parsed["surface_packets"].as_array().unwrap();
        assert_eq!(arr[0]["rule_count"], 2);
    }

    #[test]
    fn test_list_empty_when_no_packets() {
        let (_tmp, server) = make_server();

        let result = server.handle_mcp_surface_list(20, 0, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 0);
    }

    #[test]
    fn test_describe_returns_rules_and_verdict() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();

        compile_surface_packet(
            &packets,
            vec![
                surface_rule(
                    "readonly",
                    "readonly",
                    &["bbox_search"],
                    &[],
                    "tool_surface",
                ),
                catchall_deny(),
            ],
            "global",
            None,
        );
        drop(packets);

        let result = server
            .handle_mcp_surface_describe(
                &McpSurfaceParams {
                    action: McpSurfaceAction::Describe,
                    surface: Some("readonly".to_string()),
                    project: None,
                    detail: None,
                    limit: None,
                    offset: None,
                    body_limit: None,
                    cursor: None,
                },
                None,
            )
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["packet"]["rule_count"], 2);
        assert_eq!(parsed["selected_surface"], "readonly");
        assert_eq!(parsed["verdict_for_selected"]["route"], "tool_surface");

        let rules = parsed["rules"].as_array().unwrap();
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0]["matches_surface"], "readonly");
        assert_eq!(rules[0]["classification"], "tool_surface");
        assert_eq!(rules[1]["matches_surface"], "*");
        assert_eq!(rules[1]["classification"], "deny");
    }

    #[test]
    fn test_replay_policy_detail_pages_exact_patterns() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();
        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "readonly",
                "readonly",
                &["bbox_search", "bbox_stats"],
                &["bbox_secret_*"],
                "tool_surface",
            )],
            "global",
            None,
        );
        drop(packets);

        let mut params = McpSurfaceParams {
            action: McpSurfaceAction::Replay,
            surface: Some("readonly".to_string()),
            project: None,
            detail: Some(McpSurfaceDetail::Policy),
            limit: Some(1),
            offset: None,
            body_limit: None,
            cursor: None,
        };
        let result = server.handle_mcp_surface_replay(&params, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["detail"], "policy");
        assert_eq!(
            parsed["policy_order"],
            "allow_patterns_then_disallow_patterns"
        );
        assert_eq!(parsed["total"], 3);
        assert_eq!(parsed["offset"], 0);
        assert_eq!(parsed["next_offset"], 1);
        assert_eq!(parsed["policy"][0]["kind"], "allow");
        assert_eq!(parsed["policy"][0]["pattern"], "bbox_search");

        params.offset = Some(1);
        let result = server.handle_mcp_surface_replay(&params, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["policy"][0]["pattern"], "bbox_stats");
        assert_eq!(parsed["next_offset"], 2);

        params.offset = Some(2);
        let result = server.handle_mcp_surface_replay(&params, None).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["policy"][0]["kind"], "disallow");
        assert_eq!(parsed["policy"][0]["pattern"], "bbox_secret_*");
        assert_eq!(parsed["next_offset"], serde_json::Value::Null);
    }

    #[test]
    fn test_describe_policy_detail_reports_exact_reader() {
        let (_tmp, server) = make_server();
        let packets = server.state.packets.read();
        compile_surface_packet(
            &packets,
            vec![surface_rule(
                "readonly",
                "readonly",
                &["bbox_search"],
                &["bbox_secret_*"],
                "tool_surface",
            )],
            "global",
            None,
        );
        let packet_id = packets
            .load_latest_by_domain(surface::SURFACE_ROUTING_DOMAIN, None, None)
            .unwrap()
            .unwrap()
            .id;
        drop(packets);

        let result = server
            .handle_mcp_surface_describe(
                &McpSurfaceParams {
                    action: McpSurfaceAction::Describe,
                    surface: Some("readonly".to_string()),
                    project: None,
                    detail: Some(McpSurfaceDetail::Policy),
                    limit: Some(20),
                    offset: Some(0),
                    body_limit: None,
                    cursor: None,
                },
                None,
            )
            .unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["selected_surface"], "readonly");
        assert_eq!(parsed["total"], 2);
        assert_eq!(parsed["policy"][0]["pattern"], "bbox_search");
        assert_eq!(parsed["policy"][1]["pattern"], "bbox_secret_*");
        assert!(
            parsed["exact_reader"]
                .as_str()
                .unwrap()
                .contains(&format!("packet:{packet_id}"))
        );
    }

    #[tokio::test]
    async fn test_list_rejects_replay_and_describe_selectors() {
        let (_tmp, server) = make_server();

        let result = server
            .bbox_mcp_surface(Parameters(McpSurfaceParams {
                action: McpSurfaceAction::List,
                surface: Some("readonly".to_string()),
                project: None,
                detail: Some(McpSurfaceDetail::Policy),
                limit: Some(20),
                offset: Some(0),
                body_limit: None,
                cursor: None,
            }))
            .await;
        assert!(result.is_error.unwrap_or(false), "{result:?}");
        let error = &result.content[0].as_text().unwrap().text;
        assert!(error.contains("action=list"), "{error}");
    }

    #[test]
    fn test_list_stale_offset_is_rejected() {
        let (_tmp, server) = make_server();

        let error = server
            .handle_mcp_surface_list(20, 1, None)
            .unwrap_err()
            .to_string();
        assert!(error.contains("error.stale_surface_offset"), "{error}");
    }

    #[test]
    fn test_surface_page_byte_limit_recomputes_next_offset() {
        let (_tmp, server) = make_server();
        let rows: Vec<_> = (0..100)
            .map(|index| {
                serde_json::json!({
                    "index": index,
                    "payload": "p".repeat(1024),
                })
            })
            .collect();

        let mut offset = 0;
        let mut indexes = Vec::new();
        loop {
            let page = server
                .surface_page(
                    serde_json::Value::Null,
                    "rows",
                    rows.clone(),
                    offset,
                    100,
                    None,
                )
                .unwrap();
            let page: serde_json::Value = serde_json::from_str(&page).unwrap();
            let returned = page["rows"].as_array().unwrap();
            assert!(!returned.is_empty());
            indexes.extend(returned.iter().map(|row| row["index"].as_u64().unwrap()));
            offset = match page["next_offset"].as_u64() {
                Some(next) => {
                    assert_eq!(page["byte_limited"], true);
                    usize::try_from(next).unwrap()
                }
                None => {
                    assert_eq!(page["byte_limited"], serde_json::Value::Null);
                    break;
                }
            };
        }

        assert_eq!(indexes, (0..100).collect::<Vec<_>>());
    }

    #[test]
    fn test_describe_no_packet_returns_error() {
        let (_tmp, server) = make_server();

        let result = server.handle_mcp_surface_describe(
            &McpSurfaceParams {
                action: McpSurfaceAction::Describe,
                surface: Some("readonly".to_string()),
                project: None,
                detail: None,
                limit: None,
                offset: None,
                body_limit: None,
                cursor: None,
            },
            None,
        );

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("no mcp-surface/routing packet found"));
    }
}

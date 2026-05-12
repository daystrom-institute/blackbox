use crate::server::*;
use crate::*;
use rmcp::schemars;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::mcp_surface_tools()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct McpSurfaceParams {
    /// Action to perform. "replay" evaluates a surface; "list" shows
    /// installed surface packets; "describe" shows a surface packet's rules.
    pub action: String,
    /// Surface name to evaluate. Defaults to "default".
    #[serde(default)]
    pub surface: Option<String>,
    /// Optional project path for project-scoped surface packets.
    #[serde(default)]
    pub project: Option<String>,
}

#[tool_router(router = mcp_surface_tools)]
impl BlackboxServer {
    #[tool(
        name = "bbox_mcp_surface",
        description = "MCP surface debugging, listing, and inspection. Actions: 'replay' evaluates a surface selector against the routing packet; 'list' shows installed surface packets; 'describe' shows packet rules plus verdict for a selected surface."
    )]
    pub(crate) fn bbox_mcp_surface(
        &self,
        Parameters(p): Parameters<McpSurfaceParams>,
    ) -> CallToolResult {
        Self::run("bbox_mcp_surface", || match p.action.as_str() {
            "replay" => self.handle_mcp_surface_replay(&p),
            "list" => self.handle_mcp_surface_list(),
            "describe" => self.handle_mcp_surface_describe(&p),
            _ => Err(anyhow::anyhow!(
                "unknown action '{}'. Valid actions: replay, list, describe",
                p.action
            )),
        })
    }
}

impl BlackboxServer {
    fn handle_mcp_surface_replay(&self, p: &McpSurfaceParams) -> anyhow::Result<String> {
        let surface = p.surface.as_deref().unwrap_or("default");
        let entity = surface::build_surface_entity(surface, p.project.as_deref());

        let packets = self.state.packets.read();
        let decision =
            surface::evaluate_tool_surface(&packets, entity.clone(), p.project.as_deref());
        drop(packets);

        let tool_universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();

        let visible_tools: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .filter(|t| surface::tool_visible(&t.name, &decision, &tool_universe))
            .map(|t| t.name.to_string())
            .collect();

        let consequent_json = match &decision.verdict {
            surface::ToolSurfaceVerdict::ToolSurface {
                allow,
                disallow,
                instructions,
            } => serde_json::json!({
                "route": "tool_surface",
                "allow": allow,
                "disallow": disallow,
                "instructions": instructions,
            }),
            surface::ToolSurfaceVerdict::Deny { reason } => serde_json::json!({
                "route": "deny",
                "reason": reason,
            }),
        };

        let verdict_classification = match &decision.verdict {
            surface::ToolSurfaceVerdict::ToolSurface { .. } => "tool_surface",
            surface::ToolSurfaceVerdict::Deny { .. } => "deny",
        };

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "entity": entity,
            "verdict_classification": verdict_classification,
            "verdict_consequent": consequent_json,
            "visible_tools": visible_tools,
            "visible_tool_count": visible_tools.len(),
        }))?)
    }

    fn handle_mcp_surface_list(&self) -> anyhow::Result<String> {
        let packets = self.state.packets.read();
        let all = packets.list_all()?;
        let surface_packets: Vec<serde_json::Value> = all
            .into_iter()
            .filter(|p| p.domain == surface::SURFACE_ROUTING_DOMAIN)
            .map(|p| {
                serde_json::json!({
                    "id": p.id,
                    "scope": p.scope,
                    "project": p.project,
                    "created_at": p.created_at,
                    "rule_count": p.rules.len(),
                })
            })
            .collect();
        drop(packets);

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "surface_packets": surface_packets,
            "count": surface_packets.len(),
        }))?)
    }

    fn handle_mcp_surface_describe(&self, p: &McpSurfaceParams) -> anyhow::Result<String> {
        let packets = self.state.packets.read();
        let loaded =
            packets.load_latest_by_domain(surface::SURFACE_ROUTING_DOMAIN, p.project.as_deref());
        let packet = loaded?.ok_or_else(|| {
            anyhow::anyhow!(
                "no mcp-surface/routing packet found{}",
                p.project
                    .as_deref()
                    .map(|pr| format!(" for project {pr}"))
                    .unwrap_or_default()
            )
        })?;

        let rules: Vec<serde_json::Value> = packet
            .rules
            .iter()
            .map(|r| {
                let classification = r.classification.as_str();
                let matching_surface = match &r.antecedent {
                    crate::packets::Predicate::Eq { field, value } if field == "surface" => {
                        match value {
                            crate::packets::Value::String(s) => s.clone(),
                            other => format!("{:?}", other),
                        }
                    }
                    _ => "*".to_string(),
                };
                serde_json::json!({
                    "id": r.id,
                    "classification": classification,
                    "matches_surface": matching_surface,
                })
            })
            .collect();

        let packet_meta = serde_json::json!({
            "id": packet.id,
            "domain": packet.domain,
            "scope": packet.scope,
            "project": packet.project,
            "rule_count": packet.rules.len(),
        });
        drop(packets);

        let selected = p.surface.as_deref().unwrap_or("default");
        let entity = surface::build_surface_entity(selected, p.project.as_deref());
        let packets_guard = self.state.packets.read();
        let decision = surface::evaluate_tool_surface(&packets_guard, entity, p.project.as_deref());
        drop(packets_guard);

        let verdict_summary = match &decision.verdict {
            surface::ToolSurfaceVerdict::ToolSurface {
                allow, disallow, ..
            } => serde_json::json!({
                "route": "tool_surface",
                "allow": allow,
                "disallow": disallow,
            }),
            surface::ToolSurfaceVerdict::Deny { reason } => serde_json::json!({
                "route": "deny",
                "reason": reason,
            }),
        };

        Ok(serde_json::to_string_pretty(&serde_json::json!({
            "packet": packet_meta,
            "rules": rules,
            "selected_surface": selected,
            "verdict_for_selected": verdict_summary,
        }))?)
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

    #[test]
    fn test_replay_readonly_returns_filtered_tools() {
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
            .handle_mcp_surface_replay(&McpSurfaceParams {
                action: "replay".to_string(),
                surface: Some("readonly".to_string()),
                project: None,
            })
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["verdict_classification"], "tool_surface");
        assert!(parsed["visible_tools"].as_array().unwrap().len() > 0);
        let visible: Vec<&str> = parsed["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
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
            .handle_mcp_surface_replay(&McpSurfaceParams {
                action: "replay".to_string(),
                surface: Some("unknown".to_string()),
                project: None,
            })
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
            .handle_mcp_surface_replay(&McpSurfaceParams {
                action: "replay".to_string(),
                surface: Some("default".to_string()),
                project: Some(project_path.to_string()),
            })
            .unwrap();

        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["verdict_classification"], "tool_surface");
        let visible: Vec<&str> = parsed["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert!(
            visible.contains(&"bbox_stats"),
            "project-scoped packet should allow bbox_stats: {:?}",
            visible
        );

        let result_global = server
            .handle_mcp_surface_replay(&McpSurfaceParams {
                action: "replay".to_string(),
                surface: Some("default".to_string()),
                project: None,
            })
            .unwrap();

        let parsed_global: serde_json::Value = serde_json::from_str(&result_global).unwrap();
        let visible_global: Vec<&str> = parsed_global["visible_tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
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

        let result = server.handle_mcp_surface_list().unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed["count"], 1);
        let arr = parsed["surface_packets"].as_array().unwrap();
        assert_eq!(arr[0]["rule_count"], 2);
    }

    #[test]
    fn test_list_empty_when_no_packets() {
        let (_tmp, server) = make_server();

        let result = server.handle_mcp_surface_list().unwrap();
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
            .handle_mcp_surface_describe(&McpSurfaceParams {
                action: "describe".to_string(),
                surface: Some("readonly".to_string()),
                project: None,
            })
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
    fn test_describe_no_packet_returns_error() {
        let (_tmp, server) = make_server();

        let result = server.handle_mcp_surface_describe(&McpSurfaceParams {
            action: "describe".to_string(),
            surface: Some("readonly".to_string()),
            project: None,
        });

        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("no mcp-surface/routing packet found"));
    }
}

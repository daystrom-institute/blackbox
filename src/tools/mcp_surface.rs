use crate::server::*;
use crate::*;
use rmcp::schemars;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::mcp_surface_tools()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct McpSurfaceParams {
    /// Action to perform. Currently only "replay" is supported.
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
        description = "MCP surface debugging and replay tool. Use action='replay' to evaluate a surface routing packet against a surface selector and see the resulting verdict plus visible tool list. Iterates on surface rules without restarting providers."
    )]
    pub(crate) fn bbox_mcp_surface(
        &self,
        Parameters(p): Parameters<McpSurfaceParams>,
    ) -> CallToolResult {
        Self::run("bbox_mcp_surface", || match p.action.as_str() {
            "replay" => self.handle_mcp_surface_replay(&p),
            _ => Err(anyhow::anyhow!(
                "unknown action '{}'. Valid actions: replay",
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
        let decision = surface::evaluate_tool_surface(&*packets, entity.clone(), p.project.as_deref());
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
                classification_lattice: Some(vec![
                    "tool_surface".to_string(),
                    "deny".to_string(),
                ]),
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

        let result = server.handle_mcp_surface_replay(&McpSurfaceParams {
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
                surface_rule("readonly", "readonly", &["bbox_search"], &[], "tool_surface"),
                catchall_deny(),
            ],
            "global",
            None,
        );
        drop(packets);

        let result = server.handle_mcp_surface_replay(&McpSurfaceParams {
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

        let result = server.handle_mcp_surface_replay(&McpSurfaceParams {
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

        let result_global = server.handle_mcp_surface_replay(&McpSurfaceParams {
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
}

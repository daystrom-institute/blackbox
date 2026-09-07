use crate::orchestration;
use crate::server::BlackboxServer;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::config_tools()
}

#[tool_router(router = config_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_mcp",
        description = "Manage MCP servers + tool filters for dispatched bros."
    )]
    pub(crate) fn bro_mcp(
        &self,
        Parameters(p): Parameters<orchestration::mcp::McpToolParams>,
    ) -> CallToolResult {
        Self::run("bro_mcp", || {
            let mut p = p;
            orchestration::mcp::validate_selection(&p)?;
            if p.action == orchestration::mcp::McpAction::Sync {
                return orchestration::mcp::handle(&p).and_then(|reply| page_mcp_reply(reply, &p));
            }
            if p.scope.as_deref() == Some("project") && !self.state.project_authority.is_bridge() {
                anyhow::bail!(
                    "error.mcp_config_locality_required: project MCP configuration has no remote owner transport; use the checkout owner's file tools or scope=global with no project. No project configuration was read or changed"
                );
            }
            // Project selectors are meaningful only for scope=project; the
            // tool layer rejects the global+project combination explicitly,
            // so an ambiguous call never resolves a selector it would then
            // ignore.
            if p.scope.as_deref() == Some("project") {
                if let Some(raw) = p.project.clone() {
                    match self.resolve_project_selection(&raw) {
                        Ok(resolution) => match resolution.store_key() {
                            Some(key) => p.project = Some(key.to_owned()),
                            None => anyhow::bail!(
                                "error.project_attachment_required: project '{raw}' has no active checkout attachment to carry MCP project scope"
                            ),
                        },
                        Err(_) if self.state.project_authority.is_bridge() => {
                            self.state.resolver_compat.record(
                                "bro_mcp",
                                crate::server::resolver_compat::CompatLane::UnregisteredWritePassThrough,
                            );
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            orchestration::mcp::handle(&p).and_then(|reply| page_mcp_reply(reply, &p))
        })
    }
}

/// Render a `bro_mcp` reply as the complete serialized tool response. Body
/// replies wrap the exact redacted value in bounded JSON body pages whose
/// cursors bind the selection and content, so a single huge accepted record
/// (env/header-key inventory, exclude list, long name) recovers exactly
/// without exceeding the transport cap.
pub(crate) fn page_mcp_reply(
    reply: orchestration::mcp::McpToolReply,
    p: &orchestration::mcp::McpToolParams,
) -> anyhow::Result<String> {
    use orchestration::mcp::McpToolReply;
    match reply {
        McpToolReply::Text(text) => Ok(text),
        McpToolReply::Body {
            scope,
            selection,
            value,
        } => {
            let body = super::body_page::json_body_page(
                &selection,
                &value,
                p.cursor.as_deref(),
                p.body_limit,
            )?;
            Ok(serde_json::to_string(&serde_json::json!({
                "scope": scope,
                "body": body,
            }))?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_project_mcp_refuses_without_owner_transport_and_sync_stays_retired() {
        let fixture = crate::server::state::catalog_fixture::CatalogFixture::new();
        let server = fixture.server();
        for action in ["list", "get", "add", "remove", "get_filters", "sync"] {
            let mut value =
                serde_json::json!({"action":action,"scope":"project","project":"/synthetic/owner"});
            if matches!(action, "get" | "add" | "remove") {
                value["name"] = serde_json::json!("synthetic");
            }
            if action == "add" {
                value["url"] = serde_json::json!("https://unit.test/mcp");
            }
            let response = server.bro_mcp(Parameters(serde_json::from_value(value).unwrap()));
            assert_eq!(response.is_error, Some(true));
            let wire = serde_json::to_value(response).unwrap();
            let text = wire["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(if action == "sync" {
                "mcp_sync_retired"
            } else {
                "mcp_config_locality_required"
            }));
        }
    }
}

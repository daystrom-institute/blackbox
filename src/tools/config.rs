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
            if let Some(raw) = p.project.clone() {
                // Selection-class engine resolution (phase-2 §9.2 B5): a
                // resolving selector rewrites to the durable store key; the
                // version-1 arm keeps raw-path storage for unregistered
                // dirs (tagged compatibility), the catalog arm fails closed.
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
            orchestration::mcp::handle(&p)
        })
    }
}

use std::sync::Arc;

use crate::server::{self, BlackboxServer};

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, ErrorCode, InitializeRequestParams, InitializeResult,
    ListToolsResult, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool_handler};

// ---------------------------------------------------------------------------
// ServerHandler impl
// ---------------------------------------------------------------------------

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_instructions("Blackbox: unified transcript search, knowledge management, and multi-provider agent orchestration")
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let surface_str = if let Some(parts) = context.extensions.get::<http::request::Parts>() {
            server::surface::extract_surface_from_uri(parts.uri.query())
        } else {
            "default"
        };
        let entity = server::surface::build_surface_entity(surface_str, None);
        let decision = {
            let packets = self.state.packets.read();
            server::surface::evaluate_tool_surface(&packets, entity, None::<&str>)
        };
        if matches!(
            decision.verdict,
            server::surface::ToolSurfaceVerdict::Deny { .. }
        ) {
            let reason = match &decision.verdict {
                server::surface::ToolSurfaceVerdict::Deny { reason } => {
                    reason.as_deref().unwrap_or("surface denied")
                }
                _ => unreachable!(),
            };
            return Err(ErrorData::internal_error(
                format!("tool surface denied: {}", reason),
                None,
            ));
        }
        let _ = self.surface.set(Arc::from(surface_str));
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        // Canonical setter is `initialize`; fallback for paths that bypass it.
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = server::surface::build_surface_entity(surface, None);
        let packets = self.state.packets.read();
        let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
        drop(packets);
        let universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        if !server::surface::tool_visible(name, &decision, &universe) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Canonical setter is `initialize`; fallback for paths that bypass it.
        let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
        let entity = server::surface::build_surface_entity(surface, None);
        let packets = self.state.packets.read();
        let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
        drop(packets);
        let universe: Vec<String> = self
            .tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect();
        let all = self.tool_router.list_all();
        let filtered = server::surface::filter_tools(&all, &decision, &universe);
        Ok(ListToolsResult {
            tools: filtered,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let (surface, decision, universe) = {
            // Canonical setter is `initialize`; fallback for paths that bypass it.
            let surface = self.surface.get().map(|s| s.as_ref()).unwrap_or("default");
            let entity = server::surface::build_surface_entity(surface, None);
            let packets = self.state.packets.read();
            let decision = server::surface::evaluate_tool_surface(&packets, entity, None::<&str>);
            let universe: Vec<String> = self
                .tool_router
                .list_all()
                .iter()
                .map(|t| t.name.to_string())
                .collect();
            (surface.to_string(), decision, universe)
        };
        if !server::surface::tool_visible(&request.name, &decision, &universe) {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!(
                    "tool not available on surface '{}': {}",
                    surface, request.name
                ),
                None,
            ));
        }
        let tcc = ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }
}

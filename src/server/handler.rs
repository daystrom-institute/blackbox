use std::sync::Arc;

use crate::server::surface::SurfaceCacheEntry;
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

impl BlackboxServer {
    fn tool_universe(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    fn session_surface(&self) -> String {
        // Canonical setter is `initialize`; "default" covers paths that
        // bypass it.
        self.surface
            .get()
            .map(|s| s.as_ref().to_string())
            .unwrap_or_else(|| "default".to_string())
    }

    /// Surface decision for this session via the generation-validated cache.
    /// The hit path is two short lock reads; a rebuild (first request after
    /// a packet mutation) re-reads the packet store, so it runs on the
    /// blocking pool.
    async fn surface_entry(&self, surface: &str) -> Arc<SurfaceCacheEntry> {
        let generation = self.state.packets.read().generation();
        if let Some(hit) = self
            .state
            .surface_decisions
            .lookup(surface, None, generation)
        {
            return hit;
        }
        let server = self.clone();
        let surface_owned = surface.to_string();
        match tokio::task::spawn_blocking(move || {
            let universe = server.tool_universe();
            server::surface::cached_surface_entry(&server.state, &surface_owned, None, || universe)
        })
        .await
        {
            Ok(entry) => entry,
            Err(e) => {
                // Only reachable if the rebuild closure panicked; recompute
                // inline rather than poisoning the session.
                tracing::warn!(error = %e, "surface decision rebuild panicked; recomputing inline");
                self.surface_entry_sync(surface)
            }
        }
    }

    /// Synchronous variant for trait methods that cannot await. The miss
    /// path blocks on the packet store scan.
    fn surface_entry_sync(&self, surface: &str) -> Arc<SurfaceCacheEntry> {
        let universe = self.tool_universe();
        server::surface::cached_surface_entry(&self.state, surface, None, || universe)
    }
}

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
        let entry = self.surface_entry(surface_str).await;
        if let server::surface::ToolSurfaceVerdict::Deny { reason } = &entry.decision.verdict {
            let reason = reason.as_deref().unwrap_or("surface denied");
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
        let entry = self.surface_entry_sync(&self.session_surface());
        if !entry.visible.contains(name) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let entry = self.surface_entry(&self.session_surface()).await;
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| entry.visible.contains(t.name.as_ref()))
            .collect();
        Ok(ListToolsResult {
            tools,
            ..Default::default()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let surface = self.session_surface();
        let entry = self.surface_entry(&surface).await;
        if !entry.visible.contains(request.name.as_ref()) {
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

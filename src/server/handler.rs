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

    /// Session project context for surface evaluation, set at `initialize`
    /// from the `?project` query parameter (gap-310c36b6). `None` for
    /// sessions that did not select a project.
    fn session_surface_project(&self) -> Option<String> {
        self.surface_project
            .get()
            .and_then(|p| p.as_ref().map(|s| s.as_ref().to_string()))
    }

    /// Surface decision for this session via the generation-validated cache.
    /// The hit path is two short lock reads; a rebuild (first request after
    /// a packet mutation) re-reads the packet store, so it runs on the
    /// blocking pool.
    async fn surface_entry(&self, surface: &str, project: Option<&str>) -> Arc<SurfaceCacheEntry> {
        let generation = self.state.packets.read().generation();
        if let Some(hit) = self
            .state
            .surface_decisions
            .lookup(surface, project, generation)
        {
            return hit;
        }
        let server = self.clone();
        let surface_owned = surface.to_string();
        let project_owned = project.map(str::to_string);
        match tokio::task::spawn_blocking(move || {
            let universe = server.tool_universe();
            server::surface::cached_surface_entry(
                &server.state,
                &surface_owned,
                project_owned.as_deref(),
                || universe,
            )
        })
        .await
        {
            Ok(entry) => entry,
            Err(e) => {
                // Only reachable if the rebuild closure panicked; recompute
                // inline rather than poisoning the session.
                tracing::warn!(error = %e, "surface decision rebuild panicked; recomputing inline");
                self.surface_entry_sync(surface, project)
            }
        }
    }

    /// Synchronous variant for trait methods that cannot await. The miss
    /// path blocks on the packet store scan.
    fn surface_entry_sync(&self, surface: &str, project: Option<&str>) -> Arc<SurfaceCacheEntry> {
        let universe = self.tool_universe();
        server::surface::cached_surface_entry(&self.state, surface, project, || universe)
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
        let (surface_str, project_raw) =
            if let Some(parts) = context.extensions.get::<http::request::Parts>() {
                (
                    server::surface::extract_surface_from_uri(parts.uri.query()),
                    server::surface::extract_decoded_query_param(parts.uri.query(), "project"),
                )
            } else {
                ("default", None)
            };
        // Resolve the project selector (alias / id / path) to the base
        // canonical path packets are scoped by, falling back to the literal
        // value for parity with bbox_mcp_surface. Blocking fs (canonicalize
        // / git probes) → blocking pool.
        let project = match project_raw.clone() {
            Some(raw) => {
                let server = self.clone();
                let resolved = tokio::task::spawn_blocking(move || {
                    let records = server.state.projects.read().list();
                    crate::projects::resolve_project_context(
                        &raw,
                        &records,
                        crate::projects::ResolveIntent::Read,
                    )
                    .map(|ctx| ctx.host_root)
                    .unwrap_or(raw)
                })
                .await
                .map_err(|e| {
                    ErrorData::internal_error(format!("project resolution failed: {e}"), None)
                })?;
                Some(resolved)
            }
            None => None,
        };
        let entry = self.surface_entry(surface_str, project.as_deref()).await;
        if let server::surface::ToolSurfaceVerdict::Deny { reason } = &entry.decision.verdict {
            let reason = reason.as_deref().unwrap_or("surface denied");
            return Err(ErrorData::internal_error(
                format!("tool surface denied: {}", reason),
                None,
            ));
        }
        // Checkout authority is derived only after the surface decision and
        // only through the conservative write resolver. The raw transport
        // project is trusted session context; later tool arguments are not.
        let session_checkout = match project_raw {
            Some(raw) => {
                let server = self.clone();
                tokio::task::spawn_blocking(move || {
                    server
                        .resolve_project_write(&raw)
                        .ok()
                        .and_then(|resolved| resolved.checkout_scope)
                        .map(Arc::new)
                })
                .await
                .map_err(|e| {
                    ErrorData::internal_error(
                        format!("checkout authority resolution failed: {e}"),
                        None,
                    )
                })?
            }
            None => None,
        };
        let _ = self.surface.set(Arc::from(surface_str));
        let _ = self.surface_project.set(project.map(Arc::from));
        let _ = self.session_checkout.set(session_checkout);
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        let entry = self.surface_entry_sync(
            &self.session_surface(),
            self.session_surface_project().as_deref(),
        );
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
        let entry = self
            .surface_entry(
                &self.session_surface(),
                self.session_surface_project().as_deref(),
            )
            .await;
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
        let entry = self
            .surface_entry(&surface, self.session_surface_project().as_deref())
            .await;
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

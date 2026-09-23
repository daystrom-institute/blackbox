use std::collections::BTreeMap;
use std::sync::Arc;

use crate::server::surface::SurfaceCacheEntry;
use crate::server::{self, BlackboxServer};

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResult, CustomRequest, CustomResult, ErrorCode,
    GetPromptRequestParams, GetPromptResult, InitializeRequestParams, InitializeResult,
    ListPromptsResult, ListResourcesResult, ListToolsResult, PaginatedRequestParams, Prompt,
    PromptArgument, PromptMessage, PromptMessageRole, RawResource, ReadResourceRequestParams,
    ReadResourceResult, ResourceContents, ServerCapabilities, ServerInfo,
};
use rmcp::service::RequestContext;
use rmcp::{ErrorData, RoleServer, ServerHandler, tool_handler};
use serde::Deserialize;
use serde_json::json;

use super::onboarding_skill::{
    self, ONBOARDING_SKILL_DESCRIPTION, ONBOARDING_SKILL_NAME, ONBOARDING_SKILL_URI,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SkillsListParams {
    #[allow(dead_code)]
    cursor: Option<String>,
}

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
    pub(crate) fn session_surface_project(&self) -> Option<String> {
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
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "io.modelcontextprotocol/skills".to_string(),
            serde_json::Map::new(),
        );
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_extensions_with(extensions)
                .enable_prompts()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_instructions(format!(
            "Blackbox provides unified transcript search, knowledge management, and multi-provider agent orchestration. Project onboarding is described by resource {ONBOARDING_SKILL_URI}."
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        let (
            surface_str,
            project_raw,
            workspace_binding,
            operator_blame_binding,
            operator_provenance_binding,
        ) = if let Some(parts) = context.extensions.get::<http::request::Parts>() {
            let project =
                server::surface::extract_decoded_query_param(parts.uri.query(), "project")
                    .map_err(|error| {
                        ErrorData::internal_error(
                            format!("invalid project query parameter: {error}"),
                            None,
                        )
                    })?;
            let workspace_binding = match parts.headers.get(bro_protocol::WORKSPACE_BINDING_HEADER)
            {
                Some(candidate) => {
                    let candidate = candidate.to_str().map_err(|_| {
                        ErrorData::new(
                            ErrorCode::INVALID_REQUEST,
                            "invalid workspace binding",
                            None,
                        )
                    })?;
                    Some(
                        self.state
                            .knowledge_sources
                            .authenticate_workspace_binding_now(candidate)
                            .ok_or_else(|| {
                                ErrorData::new(
                                    ErrorCode::INVALID_REQUEST,
                                    "invalid workspace binding",
                                    None,
                                )
                            })?,
                    )
                }
                None => None,
            };
            let operator_blame_binding =
                server::blame_authority::authenticate_operator_blame_binding(
                    self.state.as_ref(),
                    &parts.headers,
                )
                .map_err(|message| ErrorData::new(ErrorCode::INVALID_REQUEST, message, None))?;
            let operator_provenance_binding =
                server::provenance_authority::authenticate_operator_provenance_binding(
                    self.state.as_ref(),
                    &parts.headers,
                )
                .map_err(|message| ErrorData::new(ErrorCode::INVALID_REQUEST, message, None))?;
            if [
                workspace_binding.is_some(),
                operator_blame_binding.is_some(),
                operator_provenance_binding.is_some(),
            ]
            .into_iter()
            .filter(|present| *present)
            .count()
                > 1
            {
                return Err(ErrorData::new(
                    ErrorCode::INVALID_REQUEST,
                    "managed workspace and operator checkout authorities are mutually exclusive",
                    None,
                ));
            }
            (
                server::surface::extract_surface_from_uri(parts.uri.query()),
                project,
                workspace_binding,
                operator_blame_binding,
                operator_provenance_binding,
            )
        } else {
            ("default", None, None, None, None)
        };
        // Resolve the project selector (alias / id / path) through the
        // shared engine (phase-2 §9.2, Filter class) to the base canonical
        // path packets are scoped by, keeping the literal on a miss for
        // parity with bbox_mcp_surface. A catalog-mode identity with no
        // attachment pins the stable project id: identity without a host
        // path. Blocking fs (canonicalize / git probes) → blocking pool.
        let authority_project = workspace_binding
            .as_ref()
            .map(|grant| grant.project_id.clone())
            .or_else(|| {
                operator_blame_binding
                    .as_ref()
                    .map(|grant| grant.project_id.clone())
            })
            .or_else(|| {
                operator_provenance_binding
                    .as_ref()
                    .map(|grant| grant.project_id.clone())
            });
        let project = match authority_project {
            Some(project_id) => Some(project_id),
            None => match project_raw.clone() {
                Some(raw) => {
                    let server = self.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                    match server.resolve_project_filter(&raw) {
                        Some(resolution) => match resolution
                            .store_key()
                            .or(resolution.project_id())
                            .map(str::to_owned)
                        {
                            Some(resolved) => resolved,
                            None => {
                                server.state.resolver_compat.record(
                                    "mcp_wire_head",
                                    crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                                );
                                raw
                            }
                        },
                        None => {
                            server.state.resolver_compat.record(
                                "mcp_wire_head",
                                crate::server::resolver_compat::CompatLane::UnregisteredLiteralFilter,
                            );
                            raw
                        }
                    }
                })
                .await
                .map_err(|e| {
                    ErrorData::internal_error(format!("project resolution failed: {e}"), None)
                })?;
                    Some(resolved)
                }
                None => None,
            },
        };
        let entry = self.surface_entry(surface_str, project.as_deref()).await;
        if let server::surface::ToolSurfaceVerdict::Deny { reason } = &entry.decision.verdict {
            let reason = reason.as_deref().unwrap_or("surface denied");
            return Err(ErrorData::internal_error(
                format!("tool surface denied: {}", reason),
                None,
            ));
        }
        // A raw `?project` remains a surface/filter selector only. Managed
        // workspace authority comes exclusively from the private capability
        // header minted for this supervised harness session.
        let _ = self.surface.set(Arc::from(surface_str));
        let _ = self.surface_project.set(project.map(Arc::from));
        let _ = self.session_checkout.set(None);
        let _ = self
            .session_workspace_binding
            .set(workspace_binding.map(Arc::new));
        let _ = self
            .session_operator_blame_binding
            .set(operator_blame_binding.map(Arc::new));
        let _ = self
            .session_operator_provenance_binding
            .set(operator_provenance_binding.map(Arc::new));
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

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        Ok(ListResourcesResult {
            resources: vec![rmcp::model::Annotated::new(
                RawResource::new(ONBOARDING_SKILL_URI, "onboard-project/SKILL.md")
                    .with_description(ONBOARDING_SKILL_DESCRIPTION)
                    .with_mime_type("text/markdown"),
                None,
            )],
            next_cursor: None,
            meta: None,
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, ErrorData> {
        if request.uri != ONBOARDING_SKILL_URI {
            return Err(ErrorData::resource_not_found(
                format!("resource not found: {}", request.uri),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(onboarding_skill::render(&self.state), ONBOARDING_SKILL_URI)
                .with_mime_type("text/markdown"),
        ]))
    }

    async fn list_prompts(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        Ok(ListPromptsResult {
            prompts: vec![Prompt::new(
                ONBOARDING_SKILL_NAME,
                Some(ONBOARDING_SKILL_DESCRIPTION),
                Some(vec![
                    PromptArgument::new("path")
                        .with_description("Absolute path on the checkout host")
                        .with_required(false),
                ]),
            )],
            next_cursor: None,
            meta: None,
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResult, ErrorData> {
        if request.name != ONBOARDING_SKILL_NAME {
            return Err(ErrorData::invalid_params(
                format!("unknown prompt: {}", request.name),
                None,
            ));
        }
        let path = request
            .arguments
            .as_ref()
            .and_then(|arguments| arguments.get("path"))
            .and_then(serde_json::Value::as_str);
        let target = path
            .map(|path| format!("Please onboard the project at `{path}`."))
            .unwrap_or_else(|| "Please onboard the current project.".to_string());
        let message = format!("{}\n\n{target}\n", onboarding_skill::render(&self.state));
        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            PromptMessageRole::User,
            message,
        )])
        .with_description(ONBOARDING_SKILL_DESCRIPTION))
    }

    async fn on_custom_request(
        &self,
        request: CustomRequest,
        _context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        if request.method != "skills/list" {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                request.method,
                None,
            ));
        }
        request
            .params_as::<SkillsListParams>()
            .map_err(|error| ErrorData::invalid_params(error.to_string(), None))?;
        let text = onboarding_skill::render(&self.state);
        let value = json!({
            "skills": [{
                "frontmatter": {
                    "name": ONBOARDING_SKILL_NAME,
                    "description": ONBOARDING_SKILL_DESCRIPTION,
                },
                "uri": ONBOARDING_SKILL_URI,
                "digest": onboarding_skill::digest(&text),
            }]
        });
        Ok(CustomResult::new(value))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use rmcp::model::{
        ClientRequest, GetPromptRequestParams, ReadResourceRequestParams, ResourceContents,
        ServerResult,
    };
    use rmcp::{ClientHandler, ServiceExt};
    use serde_json::{Value, json};

    use super::*;
    use crate::server::SharedState;

    struct TestClient;
    impl ClientHandler for TestClient {}

    fn test_server() -> (tempfile::TempDir, BlackboxServer) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let state = Arc::new(SharedState::for_test(&root));
        (dir, BlackboxServer::new(state))
    }

    #[test]
    fn capabilities_advertise_tools_resources_prompts_and_skills() {
        let (_dir, server) = test_server();
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert!(info.capabilities.resources.is_some());
        assert!(info.capabilities.prompts.is_some());
        assert_eq!(
            info.capabilities
                .extensions
                .as_ref()
                .and_then(|extensions| extensions.get("io.modelcontextprotocol/skills")),
            Some(&serde_json::Map::new())
        );
        assert!(
            info.instructions
                .as_deref()
                .unwrap()
                .contains(ONBOARDING_SKILL_URI)
        );
    }

    #[tokio::test]
    async fn skill_resource_and_prompts_round_trip_over_mcp_transport() {
        let (_dir, server) = test_server();
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let serving = tokio::spawn(async move {
            server
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = TestClient.serve(client_io).await.unwrap();

        let result = client
            .send_request(ClientRequest::CustomRequest(CustomRequest::new(
                "skills/list",
                Some(json!({})),
            )))
            .await
            .unwrap();
        let ServerResult::CustomResult(skills) = result else {
            panic!("unexpected custom result");
        };
        let skills: Value = skills.0;
        assert_eq!(
            skills["skills"][0]["frontmatter"]["name"],
            ONBOARDING_SKILL_NAME
        );
        assert_eq!(skills["skills"][0]["uri"], ONBOARDING_SKILL_URI);

        let resources = client.list_resources(None).await.unwrap();
        assert_eq!(resources.resources.len(), 1);
        assert_eq!(resources.resources[0].raw.uri, ONBOARDING_SKILL_URI);
        let read = client
            .read_resource(ReadResourceRequestParams::new(ONBOARDING_SKILL_URI))
            .await
            .unwrap();
        let ResourceContents::TextResourceContents {
            text, mime_type, ..
        } = &read.contents[0]
        else {
            panic!("skill resource must be text");
        };
        assert_eq!(mime_type.as_deref(), Some("text/markdown"));
        assert!(text.starts_with("---\nname: onboard-project\ndescription:"));
        assert_eq!(
            skills["skills"][0]["digest"],
            onboarding_skill::digest(text)
        );

        let prompts = client.list_prompts(None).await.unwrap();
        assert_eq!(prompts.prompts[0].name, ONBOARDING_SKILL_NAME);
        let without_path = client
            .get_prompt(GetPromptRequestParams::new(ONBOARDING_SKILL_NAME))
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&without_path)
                .unwrap()
                .contains("current project")
        );
        let with_path = client
            .get_prompt(
                GetPromptRequestParams::new(ONBOARDING_SKILL_NAME).with_arguments(
                    json!({"path":"/srv/checkouts/demo"})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            )
            .await
            .unwrap();
        assert!(
            serde_json::to_string(&with_path)
                .unwrap()
                .contains("/srv/checkouts/demo")
        );

        assert!(
            client
                .read_resource(ReadResourceRequestParams::new("blackbox://unknown"))
                .await
                .is_err()
        );
        assert!(
            client
                .send_request(ClientRequest::CustomRequest(CustomRequest::new(
                    "skills/unknown",
                    None,
                )))
                .await
                .is_err()
        );

        client.cancel().await.unwrap();
        serving.await.unwrap();
    }
}

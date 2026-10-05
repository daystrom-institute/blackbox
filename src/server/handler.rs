use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::server::{self, BlackboxServer};

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CustomRequest, CustomResult, DiscoverResult,
    ErrorCode, GetPromptRequestParams, GetPromptResponse, GetPromptResult, InitializeRequestParams,
    InitializeResult, ListPromptsResult, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, Prompt, PromptArgument, PromptMessage, ProtocolVersion,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, Role, ServerCapabilities, ServerConfig,
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

    /// Tool names visible on this session's surface. `initialize` refuses
    /// unknown surfaces; a session that bypassed it resolves `default`, and a
    /// surface missing from the table shows nothing.
    fn session_tools(&self) -> Arc<HashSet<String>> {
        self.surface_tools
            .get_or_init(|| {
                self.surface_tools_for(&self.session_surface())
                    .unwrap_or_default()
            })
            .clone()
    }

    pub(super) fn surface_tools_for(&self, surface: &str) -> Option<Arc<HashSet<String>>> {
        let config = self.state.config.read();
        let policy = config.surfaces.get(surface)?;
        Some(Arc::new(server::surface::visible_tool_set(
            policy,
            &self.tool_universe(),
        )))
    }
}

/// The scope one request is served under: the tool surface and its visible
/// set, the project filter selector, and managed-workspace authority. All of
/// it derives from transport context the client sends on every request (the
/// URL query and the workspace binding header), never from tool arguments.
pub(super) struct RequestScope {
    surface: Arc<str>,
    surface_tools: Arc<HashSet<String>>,
    project: Option<Arc<str>>,
    workspace_binding: Option<Arc<server::knowledge_source::WorkspaceBindingGrant>>,
}

impl BlackboxServer {
    /// Resolve the scope named by one request's transport context. A request
    /// with no HTTP parts (an in-process transport) is served on `default`.
    /// An unknown surface or an unauthenticated workspace binding is refused.
    pub(super) async fn resolve_request_scope(
        &self,
        parts: Option<&http::request::Parts>,
    ) -> Result<RequestScope, ErrorData> {
        let (surface_str, project_raw, workspace_binding) = if let Some(parts) = parts {
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
            (
                server::surface::extract_surface_from_uri(parts.uri.query()),
                project,
                workspace_binding,
            )
        } else {
            ("default", None, None)
        };
        let Some(surface_tools) = self.surface_tools_for(surface_str) else {
            return Err(ErrorData::new(
                ErrorCode::INVALID_REQUEST,
                format!(
                    "tool surface denied: {}",
                    server::surface::unknown_surface_message(surface_str)
                ),
                None,
            ));
        };
        // Resolve the project selector (alias / id / path) through the
        // shared engine (phase-2 §9.2, Filter class) to the base canonical
        // path, keeping the literal on a miss. A catalog-mode identity with
        // no attachment pins the stable project id: identity without a host
        // path. Blocking fs (canonicalize / git probes) → blocking pool.
        let authority_project = workspace_binding
            .as_ref()
            .map(|grant| grant.project_id.clone());
        let project = match authority_project {
            Some(project_id) => Some(project_id),
            None => match project_raw.clone() {
                Some(raw) => {
                    let server = self.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                        server
                            .resolve_project_filter(&raw)
                            .and_then(|resolution| {
                                resolution
                                    .store_key()
                                    .or(resolution.project_id())
                                    .map(str::to_owned)
                            })
                            .unwrap_or(raw)
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
        // A raw `?project` remains a surface/filter selector only. Managed
        // workspace authority comes exclusively from the private capability
        // header minted for this supervised harness session.
        Ok(RequestScope {
            surface: Arc::from(surface_str),
            surface_tools,
            project: project.map(Arc::from),
            workspace_binding: workspace_binding.map(Arc::new),
        })
    }

    /// Bind this handler instance to one scope. Every slot is set together.
    pub(super) fn pin_scope(&self, scope: RequestScope) {
        let _ = self.surface.set(scope.surface);
        let _ = self.surface_tools.set(scope.surface_tools);
        let _ = self.surface_project.set(scope.project);
        let _ = self.session_checkout.set(None);
        let _ = self.session_workspace_binding.set(scope.workspace_binding);
    }

    /// The handler a request runs on. A session keeps the scope its
    /// `initialize` pinned. A request that arrives on an unpinned handler is
    /// served by a fresh instance bound to that request's own scope, so it can
    /// neither inherit nor leave behind another request's surface, project or
    /// workspace authority.
    pub(super) async fn scoped_for(
        &self,
        parts: Option<&http::request::Parts>,
    ) -> Result<std::borrow::Cow<'_, Self>, ErrorData> {
        if self.surface.get().is_some() || parts.is_none() {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        let scope = self.resolve_request_scope(parts).await?;
        let server = self.unpinned_clone();
        server.pin_scope(scope);
        Ok(std::borrow::Cow::Owned(server))
    }

    fn unpinned_clone(&self) -> Self {
        Self {
            embed_status_snapshots: self.embed_status_snapshots.clone(),
            state: self.state.clone(),
            tool_router: self.tool_router.clone(),
            surface: Default::default(),
            surface_tools: Default::default(),
            surface_project: Default::default(),
            session_checkout: Default::default(),
            session_workspace_binding: Default::default(),
        }
    }
}

/// The newest protocol revision this wire head serves. Surface, project and
/// workspace-binding scope are pinned at `initialize`, so only revisions with
/// that handshake are supported and `server/discover` is refused.
const LEGACY_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_06_18;

#[tool_handler(router = self.tool_router)]
impl ServerHandler for BlackboxServer {
    fn get_info(&self) -> ServerConfig {
        let mut extensions = BTreeMap::new();
        extensions.insert(
            "io.modelcontextprotocol/skills".to_string(),
            serde_json::Map::new(),
        );
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_extensions_with(extensions)
                .enable_prompts()
                .enable_resources()
                .enable_tools()
                .build(),
        )
        .with_protocol_version(LEGACY_PROTOCOL_VERSION)
        .with_instructions(format!(
            "Blackbox provides unified transcript search, knowledge management, and multi-provider agent orchestration. Project onboarding is described by resource {ONBOARDING_SKILL_URI}."
        ))
    }

    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&LEGACY_PROTOCOL_VERSION))
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        Err(ErrorData::new(
            ErrorCode::METHOD_NOT_FOUND,
            "server/discover",
            None,
        ))
    }

    async fn initialize(
        &self,
        request: InitializeRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, ErrorData> {
        // Resolve everything before pinning anything: a refused scope must
        // leave no session slot set.
        let scope = self
            .resolve_request_scope(context.extensions.get::<http::request::Parts>())
            .await?;
        self.pin_scope(scope);
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        Ok(self.get_info())
    }

    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
        if !self.session_tools().contains(name) {
            return None;
        }
        self.tool_router.get(name).cloned()
    }

    async fn list_tools(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let server = self
            .scoped_for(context.extensions.get::<http::request::Parts>())
            .await?;
        let visible = server.session_tools();
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| visible.contains(t.name.as_ref()))
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
            resources: vec![
                Resource::new(ONBOARDING_SKILL_URI, "onboard-project/SKILL.md")
                    .with_description(ONBOARDING_SKILL_DESCRIPTION)
                    .with_mime_type("text/markdown"),
            ],
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        if request.uri != ONBOARDING_SKILL_URI {
            return Err(ErrorData::resource_not_found(
                format!("resource not found: {}", request.uri),
                None,
            ));
        }
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(onboarding_skill::render(&self.state), ONBOARDING_SKILL_URI)
                .with_mime_type("text/markdown"),
        ])
        .into())
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
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
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
        Ok(
            GetPromptResult::new(vec![PromptMessage::new_text(Role::User, message)])
                .with_description(ONBOARDING_SKILL_DESCRIPTION)
                .into(),
        )
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
    ) -> Result<CallToolResponse, ErrorData> {
        let server = self
            .scoped_for(context.extensions.get::<http::request::Parts>())
            .await?;
        let surface = server.session_surface();
        if !server.session_tools().contains(request.name.as_ref()) {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                format!(
                    "tool not available on surface '{}': {}",
                    surface, request.name
                ),
                None,
            ));
        }
        let tcc = ToolCallContext::new(server.as_ref(), request, context);
        server.tool_router.call(tcc).await
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

    fn request_parts(uri: &str, headers: &[(&str, &str)]) -> http::request::Parts {
        let mut request = http::Request::builder().uri(uri);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        request.body(()).unwrap().into_parts().0
    }

    #[tokio::test]
    async fn an_unpinned_request_is_served_under_its_own_scope() {
        let (_dir, server) = test_server();
        let readonly = request_parts("/mcp?surface=readonly&project=literal-project", &[]);
        let scoped = server.scoped_for(Some(&readonly)).await.unwrap();
        assert_eq!(scoped.session_surface(), "readonly");
        assert!(scoped.session_tools().contains("bbox_hybrid_search"));
        assert!(!scoped.session_tools().contains("bbox_learn"));
        assert_eq!(
            scoped.surface_project.get().unwrap().as_deref(),
            Some("literal-project")
        );
        assert!(scoped.authoritative_session_workspace_binding().is_none());

        // The shared handler stays unpinned, so the next request resolves its
        // own scope instead of inheriting this one.
        assert!(server.surface.get().is_none());
        let ops = request_parts("/mcp?surface=ops", &[]);
        let scoped = server.scoped_for(Some(&ops)).await.unwrap();
        assert_eq!(scoped.session_surface(), "ops");
        assert!(scoped.session_tools().contains("bro_exec"));
        assert_eq!(scoped.surface_project.get(), Some(&None));
        assert!(server.surface.get().is_none());
    }

    #[tokio::test]
    async fn an_unpinned_request_with_a_refused_scope_is_not_served() {
        let (_dir, server) = test_server();
        let unknown = request_parts("/mcp?surface=missing", &[]);
        let error = server.scoped_for(Some(&unknown)).await.err().unwrap();
        assert_eq!(error.code, ErrorCode::INVALID_REQUEST);
        assert!(error.message.contains("tool surface denied"), "{error:?}");

        let forged = request_parts(
            "/mcp?surface=ops",
            &[(bro_protocol::WORKSPACE_BINDING_HEADER, "not-a-grant")],
        );
        let error = server.scoped_for(Some(&forged)).await.err().unwrap();
        assert_eq!(error.code, ErrorCode::INVALID_REQUEST);
        assert_eq!(error.message, "invalid workspace binding");
        assert!(server.surface.get().is_none());
    }

    #[tokio::test]
    async fn a_session_keeps_the_scope_its_initialize_pinned() {
        let (_dir, server) = test_server();
        let readonly = request_parts("/mcp?surface=readonly", &[]);
        let scope = server.resolve_request_scope(Some(&readonly)).await.unwrap();
        server.pin_scope(scope);

        let ops = request_parts("/mcp?surface=ops", &[]);
        let scoped = server.scoped_for(Some(&ops)).await.unwrap();
        assert_eq!(scoped.session_surface(), "readonly");
        assert!(!scoped.session_tools().contains("bro_exec"));
    }

    #[tokio::test]
    async fn a_request_without_transport_context_is_served_on_default() {
        let (_dir, server) = test_server();
        let scoped = server.scoped_for(None).await.unwrap();
        assert_eq!(scoped.session_surface(), "default");
        let scope = server.resolve_request_scope(None).await.unwrap();
        server.pin_scope(scope);
        assert_eq!(server.session_surface(), "default");
        assert_eq!(server.surface_project.get(), Some(&None));
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
        assert_eq!(resources.resources[0].uri, ONBOARDING_SKILL_URI);
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

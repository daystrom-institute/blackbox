use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

use crate::server::{self, BlackboxServer};

use rmcp::handler::server::tool::ToolCallContext;
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CustomRequest, CustomResult,
    DiscoverResult, ErrorCode, GetPromptRequestParams, GetPromptResponse, GetPromptResult,
    InitializeRequestParams, InitializeResult, ListPromptsResult, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, Prompt, PromptArgument, PromptMessage,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
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

/// How long a resolved project selector is reused when the catalog has not
/// moved. The authority epoch is the real invalidation; this bound only
/// limits what a change the epoch does not carry (a checkout moved on disk)
/// can leave stale.
const PROJECT_SELECTOR_TTL: std::time::Duration = std::time::Duration::from_secs(30);
const PROJECT_SELECTOR_CACHE_ENTRIES: usize = 256;

/// Resolved `?project=` selectors for requests that arrive without a
/// session. A session resolves its selector once at `initialize`; a
/// sessionless client sends the same selector on every request, and each
/// resolution is a blocking probe. An entry is reused only while the project
/// authority epoch it was resolved under is current and it is younger than
/// [`PROJECT_SELECTOR_TTL`]. Workspace bindings are never cached: they are
/// authenticated on every request.
#[derive(Default)]
pub(crate) struct ProjectSelectorCache {
    entries: parking_lot::Mutex<std::collections::HashMap<String, CachedProjectSelector>>,
}

struct CachedProjectSelector {
    resolved: String,
    authority_epoch: u64,
    resolved_at: std::time::Instant,
}

impl ProjectSelectorCache {
    fn get(&self, raw: &str, authority_epoch: u64, now: std::time::Instant) -> Option<String> {
        let entries = self.entries.lock();
        let entry = entries.get(raw)?;
        (entry.authority_epoch == authority_epoch
            && now.saturating_duration_since(entry.resolved_at) < PROJECT_SELECTOR_TTL)
            .then(|| entry.resolved.clone())
    }

    fn put(&self, raw: String, resolved: String, authority_epoch: u64, now: std::time::Instant) {
        let mut entries = self.entries.lock();
        if entries.len() >= PROJECT_SELECTOR_CACHE_ENTRIES && !entries.contains_key(&raw) {
            entries.clear();
        }
        entries.insert(
            raw,
            CachedProjectSelector {
                resolved,
                authority_epoch,
                resolved_at: now,
            },
        );
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
        self.resolve_scope(parts, false).await
    }

    /// `reuse_project_resolution` lets a sessionless request reuse a selector
    /// resolved for an earlier one; `initialize` always resolves afresh.
    async fn resolve_scope(
        &self,
        parts: Option<&http::request::Parts>,
        reuse_project_resolution: bool,
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
                    let authority_epoch = self
                        .state
                        .records_provider
                        .records_snapshot()
                        .authority_epoch;
                    let cached = reuse_project_resolution
                        .then(|| {
                            self.state.project_selector_cache.get(
                                &raw,
                                authority_epoch,
                                std::time::Instant::now(),
                            )
                        })
                        .flatten();
                    let resolved = match cached {
                        Some(resolved) => resolved,
                        None => {
                            let server = self.clone();
                            let selector = raw.clone();
                            let resolved = tokio::task::spawn_blocking(move || {
                                server
                                    .resolve_project_filter(&selector)
                                    .and_then(|resolution| {
                                        resolution
                                            .store_key()
                                            .or(resolution.project_id())
                                            .map(str::to_owned)
                                    })
                                    .unwrap_or(selector)
                            })
                            .await
                            .map_err(|e| {
                                ErrorData::internal_error(
                                    format!("project resolution failed: {e}"),
                                    None,
                                )
                            })?;
                            if reuse_project_resolution {
                                self.state.project_selector_cache.put(
                                    raw,
                                    resolved.clone(),
                                    authority_epoch,
                                    std::time::Instant::now(),
                                );
                            }
                            resolved
                        }
                    };
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
        if self.scope_pinned() || parts.is_none() {
            return Ok(std::borrow::Cow::Borrowed(self));
        }
        let scope = self.resolve_scope(parts, true).await?;
        let server = self.unpinned_clone();
        server.pin_scope(scope);
        Ok(std::borrow::Cow::Owned(server))
    }

    /// Refuse a request whose own transport context names a scope this
    /// daemon will not serve (an unknown surface, an unauthenticated
    /// workspace binding). Methods whose answer does not vary by scope call
    /// this so a misconfigured client is refused on every method, never
    /// served a partial catalog.
    async fn refuse_unservable_scope(
        &self,
        context: &RequestContext<RoleServer>,
    ) -> Result<(), ErrorData> {
        self.scoped_for(context.extensions.get::<http::request::Parts>())
            .await
            .map(|_| ())
    }

    fn modern_lifecycle_enabled(&self) -> bool {
        self.state.config.read().daemon.mcp_modern_lifecycle
    }

    /// Whether `initialize` bound this handler to a session scope.
    fn scope_pinned(&self) -> bool {
        self.surface.get().is_some()
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

/// How long a client may treat a catalog listing as fresh. A surface's tool
/// set, the resource list and the prompt list are fixed for the daemon's
/// lifetime, so the bound only limits how long a client keeps a listing
/// across a daemon restart.
const CATALOG_LIST_TTL_MS: u64 = 300_000;

/// Whether a request is served under a revision without a handshake. A
/// session that initialized negotiated a handshake revision, and that
/// decides: the request's own version is whatever the client wrote in
/// `_meta`, so it is consulted only when no session exists.
fn serves_sessionless_revision(
    session_initialized: bool,
    request_version: Option<&ProtocolVersion>,
) -> bool {
    !session_initialized && request_version.is_some_and(|version| !version.has_initialize())
}

/// Cache hints for a catalog listing. They exist only in revisions without
/// a handshake; a client on a handshake revision receives the listing as it
/// always has. Listings depend on the caller's surface, so they are private.
///
/// A session that initialized negotiated a handshake revision, and that
/// decides: `session_initialized` wins over `request_version`, which is the
/// request's own `_meta` value and therefore whatever the client wrote.
fn catalog_cache_hints(
    session_initialized: bool,
    request_version: Option<&ProtocolVersion>,
) -> (Option<u64>, Option<CacheScope>) {
    if serves_sessionless_revision(session_initialized, request_version) {
        (Some(CATALOG_LIST_TTL_MS), Some(CacheScope::Private))
    } else {
        (None, None)
    }
}

/// The newest handshake revision this wire head serves, and the version every
/// `initialize` is answered with when the client names something newer.
const LEGACY_PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion::V_2025_06_18;

/// Revisions served when `daemon.mcp_modern_lifecycle` is on: the handshake
/// revisions above plus the sessionless one. 2025-11-25 stays out, so an
/// `initialize` naming it is still answered with [`LEGACY_PROTOCOL_VERSION`].
static MODERN_LIFECYCLE_VERSIONS: [ProtocolVersion; 4] = [
    ProtocolVersion::V_2024_11_05,
    ProtocolVersion::V_2025_03_26,
    ProtocolVersion::V_2025_06_18,
    ProtocolVersion::V_2026_07_28,
];

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
        if self.modern_lifecycle_enabled() {
            std::borrow::Cow::Borrowed(&MODERN_LIFECYCLE_VERSIONS)
        } else {
            std::borrow::Cow::Borrowed(ProtocolVersion::known_up_to(&LEGACY_PROTOCOL_VERSION))
        }
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, ErrorData> {
        if !self.modern_lifecycle_enabled() {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "server/discover",
                None,
            ));
        }
        // A client that will be refused on every method learns it here.
        self.refuse_unservable_scope(&context).await?;
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(CATALOG_LIST_TTL_MS))
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

    /// The served definition of one tool, whatever the caller's surface. The
    /// SDK calls this without a request, on a handler it built for the
    /// purpose, to read an input schema for header validation, and caches the
    /// answer for the process. It is a catalog lookup, never an authorization
    /// point: `list_tools` and `call_tool` apply the request's surface.
    fn get_tool(&self, name: &str) -> Option<rmcp::model::Tool> {
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
        // The router lists tools sorted by name, so the listing is stable
        // across requests and prompt caches keyed on it stay valid.
        let tools = self
            .tool_router
            .list_all()
            .into_iter()
            .filter(|t| visible.contains(t.name.as_ref()))
            .collect();
        let (ttl_ms, cache_scope) =
            catalog_cache_hints(self.scope_pinned(), context.protocol_version().as_ref());
        Ok(ListToolsResult {
            tools,
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, ErrorData> {
        self.refuse_unservable_scope(&context).await?;
        let (ttl_ms, cache_scope) =
            catalog_cache_hints(self.scope_pinned(), context.protocol_version().as_ref());
        Ok(ListResourcesResult {
            resources: vec![
                Resource::new(ONBOARDING_SKILL_URI, "onboard-project/SKILL.md")
                    .with_description(ONBOARDING_SKILL_DESCRIPTION)
                    .with_mime_type("text/markdown"),
            ],
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, ErrorData> {
        self.refuse_unservable_scope(&context).await?;
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
        context: RequestContext<RoleServer>,
    ) -> Result<ListPromptsResult, ErrorData> {
        self.refuse_unservable_scope(&context).await?;
        let (ttl_ms, cache_scope) =
            catalog_cache_hints(self.scope_pinned(), context.protocol_version().as_ref());
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
            ttl_ms,
            cache_scope,
            ..Default::default()
        })
    }

    async fn get_prompt(
        &self,
        request: GetPromptRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<GetPromptResponse, ErrorData> {
        self.refuse_unservable_scope(&context).await?;
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
        context: RequestContext<RoleServer>,
    ) -> Result<CustomResult, ErrorData> {
        self.refuse_unservable_scope(&context).await?;
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
        // Tools build results without a result-type discriminator, the shape
        // handshake revisions carry. A sessionless revision requires it.
        let sessionless =
            serves_sessionless_revision(self.scope_pinned(), context.protocol_version().as_ref());
        let tcc = ToolCallContext::new(server.as_ref(), request, context);
        let mut response = server.tool_router.call(tcc).await?;
        if sessionless && let CallToolResponse::Complete(result) = &mut response {
            result.result_type = Some(rmcp::model::ResultType::COMPLETE);
        }
        Ok(response)
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

    // The handler methods take a `RequestContext`, which needs a live peer and
    // cannot be built here, so scope handling is tested through `scoped_for`
    // and the hint decision through `catalog_cache_hints`; the call sites in
    // the resource, prompt and custom-request methods have no direct test.
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

    #[test]
    fn catalog_cache_hints_exist_only_for_an_uninitialized_modern_request() {
        let modern = ProtocolVersion::V_2026_07_28;
        for version in [
            None,
            Some(ProtocolVersion::V_2025_03_26),
            Some(ProtocolVersion::V_2025_06_18),
            Some(ProtocolVersion::LATEST_WITH_INITIALIZE),
        ] {
            for session_initialized in [false, true] {
                assert_eq!(
                    catalog_cache_hints(session_initialized, version.as_ref()),
                    (None, None),
                    "{session_initialized} {version:?}"
                );
            }
        }
        assert_eq!(
            catalog_cache_hints(false, Some(&modern)),
            (Some(CATALOG_LIST_TTL_MS), Some(CacheScope::Private))
        );
        // The request's own version is client-written. A session that
        // initialized keeps its listing shape whatever a request claims.
        assert_eq!(catalog_cache_hints(true, Some(&modern)), (None, None));
    }

    #[test]
    fn a_cached_project_selector_is_reused_only_under_its_epoch_and_age() {
        let cache = ProjectSelectorCache::default();
        let start = std::time::Instant::now();
        assert_eq!(cache.get("alias", 7, start), None);
        cache.put("alias".into(), "p_resolved".into(), 7, start);
        assert_eq!(cache.get("alias", 7, start).as_deref(), Some("p_resolved"));
        assert_eq!(cache.get("other", 7, start), None);
        // A moved project authority invalidates at once.
        assert_eq!(cache.get("alias", 8, start), None);
        // So does age, whatever the epoch says.
        let just_inside = start + PROJECT_SELECTOR_TTL - std::time::Duration::from_millis(1);
        assert_eq!(
            cache.get("alias", 7, just_inside).as_deref(),
            Some("p_resolved")
        );
        assert_eq!(cache.get("alias", 7, start + PROJECT_SELECTOR_TTL), None);
        // A newer resolution replaces the entry.
        cache.put("alias".into(), "p_moved".into(), 8, start);
        assert_eq!(cache.get("alias", 8, start).as_deref(), Some("p_moved"));

        // The table is bounded: filling it past the limit starts over.
        for index in 0..PROJECT_SELECTOR_CACHE_ENTRIES {
            cache.put(format!("selector-{index}"), "p".into(), 8, start);
        }
        assert!(cache.entries.lock().len() <= PROJECT_SELECTOR_CACHE_ENTRIES);
    }

    #[tokio::test]
    async fn a_sessionless_request_reuses_a_resolved_selector_and_initialize_does_not() {
        let (_dir, server) = test_server();
        let parts = request_parts("/mcp?surface=ops&project=literal-project", &[]);
        assert!(
            server
                .state
                .project_selector_cache
                .entries
                .lock()
                .is_empty()
        );
        // The session path resolves afresh and leaves nothing behind.
        server.resolve_request_scope(Some(&parts)).await.unwrap();
        assert!(
            server
                .state
                .project_selector_cache
                .entries
                .lock()
                .is_empty()
        );
        // The sessionless path records what it resolved and serves it again.
        let first = server.scoped_for(Some(&parts)).await.unwrap();
        assert_eq!(server.state.project_selector_cache.entries.lock().len(), 1);
        let second = server.scoped_for(Some(&parts)).await.unwrap();
        assert_eq!(
            first.surface_project.get().unwrap().as_deref(),
            second.surface_project.get().unwrap().as_deref()
        );
        assert_eq!(
            second.surface_project.get().unwrap().as_deref(),
            Some("literal-project")
        );
    }

    #[test]
    fn modern_revisions_are_supported_only_when_enabled() {
        let (_dir, server) = test_server();
        let legacy = server.supported_protocol_versions();
        assert!(!legacy.contains(&ProtocolVersion::V_2026_07_28));
        assert!(!legacy.contains(&ProtocolVersion::LATEST_WITH_INITIALIZE));
        server.state.config.write().daemon.mcp_modern_lifecycle = true;
        let modern = server.supported_protocol_versions();
        assert!(modern.contains(&ProtocolVersion::V_2026_07_28));
        assert!(
            !modern.contains(&ProtocolVersion::LATEST_WITH_INITIALIZE),
            "the handshake answer must not move to 2025-11-25"
        );
        assert_eq!(&modern[..3], &legacy[..]);
    }

    #[test]
    fn the_tool_catalog_lists_in_name_order() {
        let (_dir, server) = test_server();
        let names: Vec<String> = server
            .tool_router
            .list_all()
            .into_iter()
            .map(|tool| tool.name.to_string())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted);
        assert!(names.len() > 1);
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

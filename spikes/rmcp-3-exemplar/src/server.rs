use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    io,
    sync::Arc,
    time::{Duration, Instant},
};

use axum::{Router, routing::any_service};
use chrono::Utc;
use rmcp::{
    ErrorData as McpError, ServerHandler,
    model::{
        CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
        ContentBlock, CreateTaskResult, DetailedTask, DiscoverResult, ElicitRequest,
        ElicitRequestParams, ElicitationSchema, GetTaskParams, GetTaskResult, Implementation,
        InputRequest, InputRequiredResult, ListResourceTemplatesResult, ListResourcesResult,
        ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProtocolVersion,
        ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
        ResourceContents, ResourceTemplate, ServerCapabilities, ServerInfo, ServerNotification,
        Task, TaskPayload, TaskStatus, TaskStatusNotification, TaskStatusNotificationParams, Tool,
        ToolAnnotations,
    },
    service::{RequestContext, RoleServer, SubscriptionContext},
    transport::streamable_http_server::{
        StreamableHttpServerConfig, StreamableHttpService,
        session::{local::LocalSessionManager, never::NeverSessionManager},
    },
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::{
    net::TcpListener,
    sync::{Mutex, broadcast},
    task::JoinHandle,
    time::sleep,
};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

const TASK_TTL_MS: u64 = 30_000;
const LIST_TTL_MS: u64 = 1_000;
const PAGE_SIZE: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub surface: String,
    pub project: Option<String>,
}

impl Default for Scope {
    fn default() -> Self {
        Self {
            surface: "default".to_owned(),
            project: None,
        }
    }
}

#[derive(Debug, Clone)]
enum DemoEvent {
    ToolsChanged,
    TaskChanged(DetailedTask),
}

#[derive(Debug, Clone)]
struct StoredTask {
    detail: DetailedTask,
    cancellation: CancellationToken,
    expires_at: Instant,
}

#[derive(Debug)]
struct DemoState {
    tasks: Mutex<HashMap<String, StoredTask>>,
    events: broadcast::Sender<DemoEvent>,
    deploy_codec: rmcp::model::RequestStateCodec,
}

#[derive(Debug, Clone)]
pub struct DemoServer {
    state: Arc<DemoState>,
}

pub struct RunningDemoServer {
    pub base_url: String,
    pub stateless_url: String,
    cancellation: CancellationToken,
    join: JoinHandle<io::Result<()>>,
}

impl RunningDemoServer {
    pub async fn shutdown(self) -> io::Result<()> {
        self.cancellation.cancel();
        self.join
            .await
            .map_err(|error| io::Error::other(error.to_string()))?
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct DeployRoundState {
    environment: String,
}

impl Default for DemoServer {
    fn default() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            state: Arc::new(DemoState {
                tasks: Mutex::new(HashMap::new()),
                events,
                deploy_codec: rmcp::model::RequestStateCodec::new(
                    b"rmcp-3-exemplar-request-state-secret-key",
                ),
            }),
        }
    }
}

impl DemoServer {
    pub async fn spawn() -> anyhow::Result<RunningDemoServer> {
        let server = Self::default();
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let address = listener.local_addr()?;
        let cancellation = CancellationToken::new();

        // rmcp 3.1 requires a real session manager for legacy initialize clients.
        let dual_stack = StreamableHttpService::new(
            {
                let server = server.clone();
                move || Ok(server.clone())
            },
            Arc::new(LocalSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(true)
                .with_cancellation_token(cancellation.child_token()),
        );

        // This endpoint is the affinity-free NeverSessionManager configuration.
        // It intentionally cannot accept legacy initialize clients in rmcp 3.1.
        let stateless = StreamableHttpService::new(
            {
                let server = server.clone();
                move || Ok(server.clone())
            },
            Arc::new(NeverSessionManager::default()),
            StreamableHttpServerConfig::default()
                .with_legacy_session_mode(true)
                .with_cancellation_token(cancellation.child_token()),
        );

        let app = Router::new()
            .route_service("/mcp", any_service(dual_stack))
            .route_service("/stateless", any_service(stateless));
        let shutdown = cancellation.clone();
        let join = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown.cancelled_owned())
                .await
        });

        let origin = format!("http://{address}");
        Ok(RunningDemoServer {
            base_url: format!("{origin}/mcp"),
            stateless_url: format!("{origin}/stateless"),
            cancellation,
            join,
        })
    }

    fn scope(context: &RequestContext<RoleServer>) -> Scope {
        let Some(parts) = context.extensions.get::<http::request::Parts>() else {
            return Scope::default();
        };
        let mut scope = Scope::default();
        if let Some(query) = parts.uri.query() {
            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "surface" => scope.surface = value.into_owned(),
                    "project" => scope.project = Some(value.into_owned()),
                    _ => {}
                }
            }
        }
        scope
    }

    fn tool(name: &'static str, description: &'static str) -> Tool {
        Tool::new(
            name,
            description,
            Arc::new(Map::from_iter([(
                "type".to_owned(),
                Value::String("object".to_owned()),
            )])),
        )
        .with_annotations(ToolAnnotations::new().open_world(false))
    }

    fn tools_for(scope: &Scope) -> Vec<Tool> {
        let mut tools = vec![
            Self::tool("demo_deploy", "MRTR deploy confirmation"),
            Self::tool("demo_dispatch", "Create a cancellable toy task"),
            Self::tool("demo_mutate_surface", "Emit tools/list_changed"),
            Self::tool("demo_secret", "Hidden on the restricted surface"),
            Self::tool("demo_wait", "Block while emitting progress ticks"),
        ];
        if scope.surface == "restricted" {
            tools.retain(|tool| tool.name != "demo_secret");
        }
        tools.sort_by(|left, right| left.name.cmp(&right.name));
        tools
    }

    fn resource_catalog(scope: &Scope) -> Vec<Resource> {
        let mut resources = vec![
            Resource::new("demo://brofile/alpha", "brofile-alpha")
                .with_mime_type("application/json"),
            Resource::new("demo://brofile/beta", "brofile-beta").with_mime_type("application/json"),
            Resource::new("demo://brofile/gamma", "brofile-gamma")
                .with_mime_type("application/json"),
            Resource::new("demo://brofile/restricted", "brofile-restricted")
                .with_mime_type("application/json"),
        ];
        if scope.surface == "restricted" {
            resources.retain(|resource| resource.uri != "demo://brofile/restricted");
        }
        resources.sort_by(|left, right| left.uri.cmp(&right.uri));
        resources
    }

    fn page_start(request: Option<PaginatedRequestParams>) -> Result<usize, McpError> {
        request
            .and_then(|request| request.cursor)
            .map_or(Ok(0), |cursor| {
                cursor
                    .parse::<usize>()
                    .map_err(|_| McpError::invalid_params("cursor must be an integer", None))
            })
    }

    async fn create_task(&self, project: Option<String>) -> Task {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let task = Task::new(&id, TaskStatus::Working, &now, &now)
            .with_status_message(format!(
                "queued for {}",
                project.as_deref().unwrap_or("unscoped project")
            ))
            .with_ttl_ms(TASK_TTL_MS)
            .with_poll_interval_ms(25);
        let detail = DetailedTask::new(task.clone(), TaskPayload::Working);
        let cancellation = CancellationToken::new();
        self.state.tasks.lock().await.insert(
            id.clone(),
            StoredTask {
                detail: detail.clone(),
                cancellation: cancellation.clone(),
                expires_at: Instant::now() + Duration::from_millis(TASK_TTL_MS),
            },
        );
        let _ = self.state.events.send(DemoEvent::TaskChanged(detail));

        let state = self.state.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(35)).await;
            if update_task(
                &state,
                &id,
                TaskStatus::Working,
                "running fake dispatch",
                None,
            )
            .await
            .is_none()
            {
                return;
            }
            tokio::select! {
                _ = cancellation.cancelled() => {
                    update_task(&state, &id, TaskStatus::Cancelled, "cancelled cooperatively", None).await;
                }
                _ = sleep(Duration::from_millis(90)) => {
                    update_task(
                        &state,
                        &id,
                        TaskStatus::Completed,
                        "fake dispatch complete",
                        Some(json!({"content": [{"type": "text", "text": "task complete"}]})),
                    ).await;
                }
            }
        });
        task
    }

    async fn task(&self, id: &str) -> Option<DetailedTask> {
        let mut tasks = self.state.tasks.lock().await;
        tasks.retain(|_, task| task.expires_at > Instant::now());
        tasks.get(id).map(|task| task.detail.clone())
    }

    async fn call_deploy(
        &self,
        request: CallToolRequestParams,
    ) -> Result<CallToolResponse, McpError> {
        if request.input_responses.is_none() {
            let environment = request
                .arguments
                .as_ref()
                .and_then(|args| args.get("environment"))
                .and_then(Value::as_str)
                .unwrap_or("staging")
                .to_owned();
            let sealed = self
                .state
                .deploy_codec
                .seal_json(&DeployRoundState { environment })
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let schema = ElicitationSchema::builder()
                .required_bool("confirm")
                .build()
                .map_err(|error| McpError::internal_error(error.to_string(), None))?;
            let elicitation = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
                meta: None,
                message: "Confirm deploy?".to_owned(),
                requested_schema: schema,
            });
            let mut requests = BTreeMap::new();
            requests.insert(
                "deploy_confirmation".to_owned(),
                InputRequest::Elicitation(elicitation),
            );
            return Ok(InputRequiredResult::new(Some(requests), Some(sealed)).into());
        }

        let sealed = request
            .request_state
            .as_deref()
            .ok_or_else(|| McpError::invalid_params("missing requestState", None))?;
        let state: DeployRoundState = self
            .state
            .deploy_codec
            .open_json(sealed)
            .map_err(|error| McpError::invalid_params(error.to_string(), None))?;
        let confirmed = request
            .input_responses
            .as_ref()
            .and_then(|responses| responses.get("deploy_confirmation"))
            .and_then(|value| value.get("content"))
            .and_then(|value| value.get("confirm"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        Ok(CallToolResult::structured(json!({
            "deployed": confirmed,
            "environment": state.environment,
        }))
        .into())
    }
}

async fn update_task(
    state: &Arc<DemoState>,
    id: &str,
    status: TaskStatus,
    message: &str,
    result: Option<Value>,
) -> Option<()> {
    let detail = {
        let mut tasks = state.tasks.lock().await;
        let task = tasks.get_mut(id)?;
        task.detail.task.status = status;
        task.detail.task.status_message = Some(message.to_owned());
        task.detail.task.last_updated_at = Utc::now().to_rfc3339();
        task.detail.payload = match status {
            TaskStatus::Working => TaskPayload::Working,
            TaskStatus::Completed => TaskPayload::Completed {
                result: result
                    .and_then(|value| value.as_object().cloned())
                    .unwrap_or_default(),
            },
            TaskStatus::Cancelled => TaskPayload::Cancelled,
            TaskStatus::Failed => TaskPayload::Failed {
                error: Map::from_iter([("message".to_owned(), Value::String(message.to_owned()))]),
            },
            TaskStatus::InputRequired => TaskPayload::InputRequired {
                input_requests: BTreeMap::new(),
            },
            _ => TaskPayload::Working,
        };
        task.detail.clone()
    };
    let _ = state.events.send(DemoEvent::TaskChanged(detail));
    Some(())
}

impl ServerHandler for DemoServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Owned(vec![
            ProtocolVersion::V_2025_11_25,
            ProtocolVersion::V_2026_07_28,
        ])
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_resources()
                .enable_resources_list_changed()
                .enable_tools()
                .enable_tool_list_changed()
                .enable_tasks()
                .build(),
        )
        .with_protocol_version(ProtocolVersion::V_2026_07_28)
        .with_server_info(Implementation::new(
            "rmcp-3-exemplar",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions("MCP 2026-07-28 mechanics exemplar")
    }

    async fn discover(
        &self,
        context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        let scope = Self::scope(&context);
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(LIST_TTL_MS)
        .with_cache_scope(CacheScope::Private)
        .with_server_info(
            Implementation::new("rmcp-3-exemplar", env!("CARGO_PKG_VERSION"))
                .with_description(format!("surface={}", scope.surface)),
        ))
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(
            ListToolsResult::with_all_items(Self::tools_for(&Self::scope(&context)))
                .with_ttl_ms(LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private),
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        Self::tools_for(&Scope::default())
            .into_iter()
            .find(|tool| tool.name == name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let scope = Self::scope(&context);
        if !Self::tools_for(&scope)
            .iter()
            .any(|tool| tool.name == request.name)
        {
            return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "tool {} is not visible on surface {}",
                request.name, scope.surface
            ))])
            .into());
        }
        match request.name.as_ref() {
            "demo_dispatch" => {
                let task = self.create_task(scope.project).await;
                if context
                    .client_capabilities()
                    .is_some_and(|capabilities| capabilities.supports_tasks())
                {
                    Ok(CreateTaskResult::new(task).into())
                } else {
                    Ok(CallToolResult::structured(json!({
                        "taskId": task.task_id,
                        "mode": "plain-json",
                    }))
                    .into())
                }
            }
            "demo_mutate_surface" => {
                let _ = self.state.events.send(DemoEvent::ToolsChanged);
                Ok(CallToolResult::structured(json!({"mutated": true})).into())
            }
            "demo_wait" => {
                let token = context.meta.get_progress_token();
                for tick in 1..=3 {
                    sleep(Duration::from_millis(20)).await;
                    if let Some(token) = token.clone() {
                        context
                            .peer
                            .notify_progress(
                                ProgressNotificationParam::new(token, tick as f64)
                                    .with_total(3.0)
                                    .with_message(format!("tick {tick}")),
                            )
                            .await
                            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                    }
                }
                Ok(CallToolResult::structured(json!({
                    "progressTokenEcho": token,
                    "ticks": 3,
                }))
                .into())
            }
            "demo_deploy" => self.call_deploy(request).await,
            "demo_secret" => Ok(CallToolResult::structured(json!({"secret": "visible"})).into()),
            _ => Err(McpError::invalid_params("unknown tool", None)),
        }
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.task(&request.task_id)
            .await
            .map(GetTaskResult::new)
            .ok_or_else(|| McpError::invalid_params("unknown or expired task", None))
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        let tasks = self.state.tasks.lock().await;
        let task = tasks
            .get(&request.task_id)
            .ok_or_else(|| McpError::invalid_params("unknown task", None))?;
        task.cancellation.cancel();
        Ok(())
    }

    fn accepted_subscription_filter(
        &self,
        requested: &rmcp::model::SubscriptionFilter,
    ) -> Option<rmcp::model::SubscriptionFilter> {
        Some(requested.clone())
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let mut events = self.state.events.subscribe();
        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                event = events.recv() => match event {
                    Ok(DemoEvent::ToolsChanged) => {
                        let _ = context.sink().notify_tool_list_changed().await;
                    }
                    Ok(DemoEvent::TaskChanged(task)) => {
                        // rmcp 3.1 rejects task notifications in SubscriptionSink.
                        // Attach the subscription ID and send through the request peer.
                        let notification = ServerNotification::TaskStatusNotification(
                            TaskStatusNotification::new(TaskStatusNotificationParams::new(task)),
                        );
                        context
                            .request_context()
                            .peer
                            .send_notification(notification)
                            .await
                            .map_err(|error| McpError::internal_error(error.to_string(), None))?;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }

    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        Ok(ListResourceTemplatesResult::with_all_items(vec![
            ResourceTemplate::new("demo://brofile/{name}", "brofile"),
            ResourceTemplate::new("demo://task/{id}", "task"),
        ])
        .with_ttl_ms(LIST_TTL_MS)
        .with_cache_scope(CacheScope::Private))
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let start = Self::page_start(request)?;
        let resources = Self::resource_catalog(&Self::scope(&context));
        let end = (start + PAGE_SIZE).min(resources.len());
        let mut result =
            ListResourcesResult::with_all_items(resources.get(start..end).unwrap_or(&[]).to_vec())
                .with_ttl_ms(LIST_TTL_MS)
                .with_cache_scope(CacheScope::Private);
        if end < resources.len() {
            result.next_cursor = Some(end.to_string());
        }
        Ok(result)
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let scope = Self::scope(&context);
        let visible = Self::resource_catalog(&scope)
            .iter()
            .any(|resource| resource.uri == request.uri);
        let body = if visible {
            json!({
                "uri": request.uri,
                "surface": scope.surface,
                "project": scope.project,
            })
        } else if let Some(id) = request.uri.strip_prefix("demo://task/") {
            serde_json::to_value(
                self.task(id)
                    .await
                    .ok_or_else(|| McpError::invalid_params("unknown task resource", None))?,
            )
            .map_err(|error| McpError::internal_error(error.to_string(), None))?
        } else {
            return Err(McpError::invalid_params(
                "resource is hidden or unknown",
                None,
            ));
        };
        Ok(ReadResourceResult::new(vec![
            ResourceContents::text(body.to_string(), request.uri)
                .with_mime_type("application/json"),
        ])
        .with_ttl_ms(LIST_TTL_MS)
        .with_cache_scope(CacheScope::Private)
        .into())
    }
}

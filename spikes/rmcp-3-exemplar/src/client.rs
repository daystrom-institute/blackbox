use std::sync::Arc;

use rmcp::{
    ClientHandler,
    model::{
        ClientCapabilities, ClientInfo, ElicitRequestParams, ElicitResult, ElicitationAction,
        ElicitationCapability, FormElicitationCapability, Implementation,
        ProgressNotificationParam, ProtocolVersion, TaskStatusNotificationParams,
    },
    service::{
        ClientLifecycleMode, ClientServiceExt, NotificationContext, RoleClient, RunningService,
    },
    transport::StreamableHttpClientTransport,
};
use serde_json::json;
use tokio::sync::Mutex;

pub type DemoClient = RunningService<RoleClient, DemoClientHandler>;

#[derive(Debug, Clone)]
pub struct DemoClientHandler {
    pub progress: Arc<Mutex<Vec<ProgressNotificationParam>>>,
    pub task_updates: Arc<Mutex<Vec<TaskStatusNotificationParams>>>,
    pub tool_list_changes: Arc<Mutex<usize>>,
    tasks: bool,
    protocol_version: ProtocolVersion,
}

impl Default for DemoClientHandler {
    fn default() -> Self {
        Self {
            progress: Arc::default(),
            task_updates: Arc::default(),
            tool_list_changes: Arc::default(),
            tasks: true,
            protocol_version: ProtocolVersion::V_2026_07_28,
        }
    }
}

impl DemoClientHandler {
    pub fn with_tasks() -> Self {
        Self::default()
    }

    pub fn without_tasks() -> Self {
        Self {
            tasks: false,
            ..Self::default()
        }
    }

    pub fn legacy_without_tasks() -> Self {
        Self {
            tasks: false,
            protocol_version: ProtocolVersion::V_2025_11_25,
            ..Self::default()
        }
    }

    pub async fn connect(
        self,
        url: &str,
        lifecycle: ClientLifecycleMode,
    ) -> anyhow::Result<DemoClient> {
        Ok(self
            .serve_with_lifecycle(StreamableHttpClientTransport::from_uri(url), lifecycle)
            .await?)
    }

    pub fn auto_mode() -> ClientLifecycleMode {
        ClientLifecycleMode::Auto {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        }
    }

    pub fn discover_mode() -> ClientLifecycleMode {
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        }
    }
}

impl ClientHandler for DemoClientHandler {
    fn get_info(&self) -> ClientInfo {
        let builder = ClientCapabilities::builder().enable_elicitation_with(
            ElicitationCapability::new()
                .with_form(FormElicitationCapability::new().with_schema_validation(true)),
        );
        let capabilities = if self.tasks {
            builder.enable_tasks().build()
        } else {
            builder.build()
        };
        ClientInfo::new(
            capabilities,
            Implementation::new("rmcp-3-exemplar-client", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(self.protocol_version.clone())
    }

    async fn create_elicitation(
        &self,
        _request: ElicitRequestParams,
        _context: rmcp::service::RequestContext<RoleClient>,
    ) -> Result<ElicitResult, rmcp::ErrorData> {
        Ok(ElicitResult::new(ElicitationAction::Accept).with_content(json!({"confirm": true})))
    }

    async fn on_progress(
        &self,
        params: ProgressNotificationParam,
        _context: NotificationContext<RoleClient>,
    ) {
        self.progress.lock().await.push(params);
    }

    async fn on_task_status(
        &self,
        params: TaskStatusNotificationParams,
        _context: NotificationContext<RoleClient>,
    ) {
        self.task_updates.lock().await.push(params);
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        *self.tool_list_changes.lock().await += 1;
    }
}

//! Remote deadlines do not prove remote cancellation. An uncertain call makes
//! its connection unusable until the operator reconciles the external outcome.

use super::*;
use rmcp::RoleClient;
use rmcp::model::{
    CallToolRequest, ClientRequest, ListResourceTemplatesRequest, ListResourcesRequest,
    PaginatedRequestParams, ReadResourceRequest, ReadResourceRequestParams, ServerResult,
};
use rmcp::service::{PeerRequestOptions, RunningService, ServiceError};
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum UncertainReason {
    Deadline,
    CancelledWait,
    ConnectionLost,
    Quarantined,
}

impl UncertainReason {
    fn label(self) -> &'static str {
        match self {
            Self::Deadline => "deadline_exceeded",
            Self::CancelledWait => "cancelled_wait",
            Self::ConnectionLost => "connection_or_protocol_failure",
            Self::Quarantined => "connection_quarantined",
        }
    }
}

pub(super) struct ServerConn {
    running: RunningService<RoleClient, ()>,
    server: String,
    tool_timeout: Duration,
    uncertain: Mutex<Option<UncertainReason>>,
    quarantined: CancellationToken,
    resources: bool,
}

impl ServerConn {
    pub(super) fn new(
        running: RunningService<RoleClient, ()>,
        server: String,
        tool_timeout_ms: u64,
    ) -> Self {
        let resources = running
            .peer()
            .peer_info()
            .is_some_and(|info| info.capabilities.resources.is_some());
        Self {
            running,
            server,
            tool_timeout: Duration::from_millis(tool_timeout_ms),
            uncertain: Mutex::new(None),
            quarantined: CancellationToken::new(),
            resources,
        }
    }

    pub(super) async fn list_tools(&self) -> anyhow::Result<Vec<rmcp::model::Tool>> {
        Ok(self.running.peer().list_all_tools().await?)
    }

    /// Whether the server declared the MCP resources capability at initialize.
    pub(super) fn resources_supported(&self) -> bool {
        self.resources
    }

    /// Cancel the underlying service. Test/close hook: mirrors what dropping
    /// the last handle does, but observably.
    pub(super) fn shutdown(&self) {
        self.running.cancellation_token().cancel();
    }

    pub(super) fn uncertain_outcome(&self) -> Option<String> {
        self.uncertain
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .map(|reason| {
                format!(
                    "MCP server {}: {}; remote completion unknown",
                    self.server,
                    reason.label()
                )
            })
    }

    fn quarantine(&self, reason: UncertainReason) {
        let mut state = self
            .uncertain
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.get_or_insert(reason);
        self.quarantined.cancel();
    }

    fn uncertain_result(
        &self,
        tool: &str,
        reason: UncertainReason,
        cancellation_requested: bool,
        protocol_error: Option<Value>,
    ) -> ToolResult {
        let not_started = matches!(reason, UncertainReason::Quarantined);
        let text = if not_started {
            "This call was not sent: the MCP server connection is quarantined after an earlier call with unknown completion. Do not retry until the operator reconciles the remote effects. A fresh session alone does not establish whether the earlier call ran."
        } else {
            "Remote tool completion is unknown. A cancellation notification cannot prove that the server stopped or that no effects occurred. The server connection is quarantined. Do not retry until the operator reconciles the remote effects."
        };
        ToolResult::Error(serde_json::json!({
            "content":[{"type":"text","text":text}],
            "structuredContent":{
                "code":if not_started {"mcp_server_quarantined"} else {"mcp_remote_outcome_unknown"},
                "server":self.server, "tool":tool, "reason":reason.label(),
                "completion":if not_started {"not_started"} else {"unknown"},
                "prior_completion_unknown":not_started,
                "server_quarantined":true, "retry_safe":false,
                "cancellation_requested":cancellation_requested,
                "remote_stop_confirmed":false,
                "protocol_error":protocol_error
            },
            "isError":true
        }).to_string())
    }

    pub(super) async fn call_tool(
        &self,
        params: CallToolRequestParams,
        cancellation: &CancellationToken,
    ) -> ToolResult {
        let tool = params.name.to_string();
        if self.quarantined.is_cancelled() {
            return self.uncertain_result(&tool, UncertainReason::Quarantined, false, None);
        }
        if cancellation.is_cancelled() {
            return result::from_native_result(ToolResult::Error(
                "MCP call cancelled before remote dispatch; this call was not sent".into(),
            ));
        }
        // One absolute deadline covers queue admission and the response. The
        // bounded cancellation-notification attempt can add at most 250 ms.
        let deadline = tokio::time::Instant::now() + self.tool_timeout;
        let request = self.running.peer().send_cancellable_request(
            CallToolRequest::new(params).into(),
            PeerRequestOptions::no_options(),
        );
        let sent = tokio::select! {
            biased;
            // The send future may already have queued a request before its
            // handle becomes observable. Only the preflight can say not sent.
            _ = self.quarantined.cancelled() => Err(UncertainReason::ConnectionLost),
            _ = cancellation.cancelled() => Err(UncertainReason::CancelledWait),
            _ = tokio::time::sleep_until(deadline) => Err(UncertainReason::Deadline),
            result = request => result.map_err(|_| UncertainReason::ConnectionLost),
        };
        let mut handle = match sent {
            Ok(handle) => handle,
            Err(reason) => {
                self.quarantine(reason);
                return self.uncertain_result(&tool, reason, false, None);
            }
        };
        let response = tokio::select! {
            biased;
            response = &mut handle.rx => Some(response.unwrap_or(Err(ServiceError::TransportClosed))),
            _ = cancellation.cancelled() => None,
            _ = self.quarantined.cancelled() => None,
            _ = tokio::time::sleep_until(deadline) => None,
        };
        let response_received = response.is_some();
        let protocol_error = match &response {
            Some(Err(ServiceError::McpError(error))) => {
                // These protocol errors reject parsing or admission. Internal
                // and server-defined failures can follow partial effects.
                if matches!(error.code.0, -32700 | -32600 | -32601 | -32602) {
                    return ToolResult::Error(
                        serde_json::json!({
                            "content":[{"type":"text","text":error.message}],
                            "structuredContent":{
                                "code":"mcp_protocol_rejection", "server":self.server,
                                "tool":tool, "protocol_error":error,
                                "completion":"rejected", "server_quarantined":false
                            },
                            "isError":true
                        })
                        .to_string(),
                    );
                }
                Some(serde_json::json!(error))
            }
            _ => None,
        };
        if let Some(Ok(ServerResult::CallToolResult(response))) = response {
            return result::to_tool_result(&response);
        }
        let reason = if response_received {
            UncertainReason::ConnectionLost
        } else if cancellation.is_cancelled() {
            UncertainReason::CancelledWait
        } else if self.quarantined.is_cancelled() {
            UncertainReason::Quarantined
        } else {
            UncertainReason::Deadline
        };
        // This call already has a request ID. Even if another call quarantined
        // the server first, its own remote execution remains unknown.
        let reason = if matches!(reason, UncertainReason::Quarantined) {
            UncertainReason::ConnectionLost
        } else {
            reason
        };
        self.quarantine(reason);
        let cancellation_requested = matches!(
            tokio::time::timeout(
                Duration::from_millis(250),
                handle.cancel(Some(
                    "harness stopped waiting; remote completion unknown".into()
                ))
            )
            .await,
            Ok(Ok(()))
        );
        self.uncertain_result(&tool, reason, cancellation_requested, protocol_error)
    }
}

/// Failure modes of a read-only resource RPC (`resources/list`,
/// `resources/templates/list`, `resources/read`).
pub(super) enum ResourceRpcError {
    /// The request was never dispatched (cancelled pre-send or an already
    /// quarantined connection refused it).
    NotSent(String),
    /// The server returned a terminal JSON-RPC error or an unusable result.
    /// Unlike `tools/call`, a received terminal response proves this read-only
    /// operation completed with a known outcome, so the connection is not
    /// quarantined and the failure surfaces as an explicit error.
    Server {
        message: String,
        protocol_error: Value,
    },
    /// Remote completion is unknown (deadline, cancellation while waiting,
    /// transport loss). The connection is quarantined and the envelope is
    /// the call's result.
    Uncertain(ToolResult),
}

impl ServerConn {
    async fn resource_request(
        &self,
        operation: &'static str,
        request: ClientRequest,
        cancellation: &CancellationToken,
    ) -> Result<ServerResult, ResourceRpcError> {
        if self.quarantined.is_cancelled() {
            return Err(ResourceRpcError::Uncertain(self.uncertain_resource_result(
                operation,
                UncertainReason::Quarantined,
                false,
                None,
            )));
        }
        if cancellation.is_cancelled() {
            return Err(ResourceRpcError::NotSent(format!(
                "MCP resource operation {operation} cancelled before remote dispatch; this call was not sent"
            )));
        }
        let deadline = tokio::time::Instant::now() + self.tool_timeout;
        let sent = tokio::select! {
            biased;
            _ = self.quarantined.cancelled() => Err(UncertainReason::ConnectionLost),
            _ = cancellation.cancelled() => Err(UncertainReason::CancelledWait),
            _ = tokio::time::sleep_until(deadline) => Err(UncertainReason::Deadline),
            result = self.running.peer().send_cancellable_request(
                request,
                PeerRequestOptions::no_options(),
            ) => result.map_err(|_| UncertainReason::ConnectionLost),
        };
        let mut handle = match sent {
            Ok(handle) => handle,
            Err(reason) => {
                self.quarantine(reason);
                return Err(ResourceRpcError::Uncertain(
                    self.uncertain_resource_result(operation, reason, false, None),
                ));
            }
        };
        let response = tokio::select! {
            biased;
            response = &mut handle.rx => Some(response.unwrap_or(Err(ServiceError::TransportClosed))),
            _ = cancellation.cancelled() => None,
            _ = self.quarantined.cancelled() => None,
            _ = tokio::time::sleep_until(deadline) => None,
        };
        let response_received = response.is_some();
        if let Some(Err(ServiceError::McpError(error))) = &response {
            // A received terminal response proves this read-only operation
            // completed (rejected before execution, or failed after it); the
            // outcome is known either way, so no quarantine applies.
            return Err(ResourceRpcError::Server {
                message: error.message.to_string(),
                protocol_error: serde_json::json!(error),
            });
        }
        if let Some(Ok(result)) = response {
            return Ok(result);
        }
        let reason = if response_received {
            UncertainReason::ConnectionLost
        } else if cancellation.is_cancelled() {
            UncertainReason::CancelledWait
        } else if self.quarantined.is_cancelled() {
            UncertainReason::Quarantined
        } else {
            UncertainReason::Deadline
        };
        let reason = if matches!(reason, UncertainReason::Quarantined) {
            UncertainReason::ConnectionLost
        } else {
            reason
        };
        self.quarantine(reason);
        let cancellation_requested = matches!(
            tokio::time::timeout(
                Duration::from_millis(250),
                handle.cancel(Some(
                    "harness stopped waiting; remote completion unknown".into()
                ))
            )
            .await,
            Ok(Ok(()))
        );
        Err(ResourceRpcError::Uncertain(self.uncertain_resource_result(
            operation,
            reason,
            cancellation_requested,
            None,
        )))
    }

    fn uncertain_resource_result(
        &self,
        operation: &str,
        reason: UncertainReason,
        cancellation_requested: bool,
        protocol_error: Option<Value>,
    ) -> ToolResult {
        let not_started = matches!(reason, UncertainReason::Quarantined);
        let text = if not_started {
            "This call was not sent: the MCP server connection is quarantined after an earlier call with unknown completion. Do not retry until the operator reconciles the remote effects. A fresh session alone does not establish whether the earlier call ran."
        } else {
            "Remote resource completion is unknown. A cancellation notification cannot prove that the server stopped or that no effects occurred. The server connection is quarantined. Do not retry until the operator reconciles the remote effects."
        };
        ToolResult::Error(serde_json::json!({
            "content":[{"type":"text","text":text}],
            "structuredContent":{
                "code":if not_started {"mcp_server_quarantined"} else {"mcp_remote_outcome_unknown"},
                "server":self.server, "operation":operation, "reason":reason.label(),
                "completion":if not_started {"not_started"} else {"unknown"},
                "prior_completion_unknown":not_started,
                "server_quarantined":true, "retry_safe":false,
                "cancellation_requested":cancellation_requested,
                "remote_stop_confirmed":false,
                "protocol_error":protocol_error
            },
            "isError":true
        }).to_string())
    }

    fn page_items(result: ServerResult) -> Result<(Vec<Value>, Option<String>), String> {
        let (entries, next_cursor) = match result {
            ServerResult::ListResourcesResult(page) => {
                (Self::serialize_entries(page.resources)?, page.next_cursor)
            }
            ServerResult::ListResourceTemplatesResult(page) => (
                Self::serialize_entries(page.resource_templates)?,
                page.next_cursor,
            ),
            _ => {
                return Err(
                    "server returned an unexpected result variant for the resource listing".into(),
                );
            }
        };
        Ok((entries, next_cursor))
    }

    /// Serialize one page's typed entries into raw JSON values, stripping the
    /// protocol-private `_meta` from each entry. Each match arm calls this
    /// with its own concrete item type before the results share a tuple type.
    fn serialize_entries<T: serde::Serialize>(items: Vec<T>) -> Result<Vec<Value>, String> {
        let mut entries = Vec::with_capacity(items.len());
        for item in items {
            let mut value = serde_json::to_value(item)
                .map_err(|error| format!("resource serialization failed: {error}"))?;
            if let Some(object) = value.as_object_mut() {
                object.remove("_meta");
            }
            entries.push(value);
        }
        Ok(entries)
    }

    pub(super) async fn list_resources(
        &self,
        cursor: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<Value>, Option<String>), ResourceRpcError> {
        let params = PaginatedRequestParams::default().with_cursor(cursor);
        let request: ClientRequest = ListResourcesRequest::with_param(params).into();
        self.resource_rpc("resources/list", request, cancellation, Self::page_items)
            .await
    }

    pub(super) async fn list_resource_templates(
        &self,
        cursor: Option<String>,
        cancellation: &CancellationToken,
    ) -> Result<(Vec<Value>, Option<String>), ResourceRpcError> {
        let params = PaginatedRequestParams::default().with_cursor(cursor);
        let request: ClientRequest = ListResourceTemplatesRequest::with_param(params).into();
        self.resource_rpc(
            "resources/templates/list",
            request,
            cancellation,
            Self::page_items,
        )
        .await
    }

    pub(super) async fn read_resource(
        &self,
        uri: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<Value>, ResourceRpcError> {
        let request: ClientRequest =
            ReadResourceRequest::new(ReadResourceRequestParams::new(uri.to_owned())).into();
        self.resource_rpc(
            "resources/read",
            request,
            cancellation,
            |result| match result {
                ServerResult::ReadResourceResult(page) => {
                    let mut contents = Vec::with_capacity(page.contents.len());
                    for content in page.contents {
                        let mut value = serde_json::to_value(content).map_err(|error| {
                            format!("resource content serialization failed: {error}")
                        })?;
                        if let Some(object) = value.as_object_mut() {
                            object.remove("_meta");
                        }
                        contents.push(value);
                    }
                    Ok(contents)
                }
                _ => Err("server returned an unexpected result variant for resources/read".into()),
            },
        )
        .await
    }

    async fn resource_rpc<T>(
        &self,
        operation: &'static str,
        request: ClientRequest,
        cancellation: &CancellationToken,
        project: fn(ServerResult) -> Result<T, String>,
    ) -> Result<T, ResourceRpcError> {
        match self
            .resource_request(operation, request, cancellation)
            .await
        {
            Ok(result) => project(result).map_err(|message| ResourceRpcError::Server {
                message: format!("{operation} failed: {message}"),
                protocol_error: Value::Null,
            }),
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bro_capabilities::{ToolCapability, ToolInvocation};
    use serde_json::json;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    use tokio::sync::Notify;

    struct Fixture {
        connection: Arc<ServerConn>,
        requests: Arc<Mutex<Vec<Value>>>,
        called: Arc<Notify>,
        cancelled: Arc<Notify>,
        task: tokio::task::JoinHandle<()>,
    }

    async fn fixture(timeout_ms: u64, respond: bool) -> Fixture {
        fixture_with_error(timeout_ms, respond, None).await
    }

    async fn fixture_with_error(
        timeout_ms: u64,
        respond: bool,
        error_code: Option<i32>,
    ) -> Fixture {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = requests.clone();
        let called = Arc::new(Notify::new());
        let call_signal = called.clone();
        let cancelled = Arc::new(Notify::new());
        let cancel_signal = cancelled.clone();
        let task = tokio::spawn(async move {
            let (read, mut write) = tokio::io::split(server);
            let mut lines = tokio::io::BufReader::new(read).lines();
            while let Some(line) = lines.next_line().await.unwrap() {
                let request: Value = serde_json::from_str(&line).unwrap();
                captured.lock().unwrap().push(request.clone());
                let result = match request["method"].as_str().unwrap() {
                    "initialize" => {
                        json!({"protocolVersion":request["params"]["protocolVersion"],"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
                    }
                    "tools/call" => {
                        call_signal.notify_one();
                        if !respond {
                            continue;
                        }
                        json!({"content":[{"type":"text","text":"completed"}],"isError":false})
                    }
                    "notifications/cancelled" => {
                        cancel_signal.notify_one();
                        continue;
                    }
                    "notifications/initialized" => continue,
                    method => panic!("unexpected fixture method {method}"),
                };
                let response = if request["method"] == "tools/call" && error_code.is_some() {
                    json!({"jsonrpc":"2.0","id":request["id"],"error":{
                        "code":error_code.unwrap(), "message":"synthetic rejection",
                        "data":{"field":"fixture"}
                    }})
                } else {
                    json!({"jsonrpc":"2.0","id":request["id"],"result":result})
                };
                write
                    .write_all(format!("{response}\n").as_bytes())
                    .await
                    .unwrap();
            }
        });
        let running = ().serve(client).await.unwrap();
        Fixture {
            connection: Arc::new(ServerConn::new(running, "fixture".into(), timeout_ms)),
            requests,
            called,
            cancelled,
            task,
        }
    }

    fn tool(connection: Arc<ServerConn>, name: &str) -> Arc<dyn Tool> {
        Arc::new(McpTool {
            backend: McpBackend::Remote(connection),
            call_name: "pending".into(),
            name: name.into(),
            description: "Remote fixture".into(),
            schema: json!({"type":"object"}),
            output_schema: json!({"type":"object"}),
            annotations: Default::default(),
        })
    }

    fn cx(root: std::path::PathBuf) -> ToolCx {
        ToolCx {
            tool_observations: Default::default(),
            instruction_generation: 0,
            instruction_policy: None,
            root,
            output_budget: 16 * 1024,
            cancellation: Default::default(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(Default::default())),
            shell_sessions: Arc::new(Mutex::new(Default::default())),
            edits: Arc::new(Mutex::new(Default::default())),
            session_env: Arc::new(Default::default()),
            child_env: Arc::new(Default::default()),
            shell_env: Arc::new(Default::default()),
            tool_arg_defaults: Arc::new(Default::default()),
        }
    }

    async fn close(fixture: Fixture) {
        fixture.connection.running.cancellation_token().cancel();
        tokio::time::timeout(Duration::from_secs(2), fixture.task)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn timeout_survives_nested_error_and_quarantines_all_aliases() {
        let fixture = fixture(20, false).await;
        let remote = tool(fixture.connection.clone(), "mcp__fixture__pending");
        let alias = tool(fixture.connection.clone(), "fixture_alias");
        let directory = tempfile::tempdir().unwrap();
        let cx = cx(directory.path().canonicalize().unwrap());
        let host = Arc::new(crate::capabilities::HostTools::new(
            vec![remote.clone(), alias.clone()],
            cx.clone(),
        ));
        let cells = crate::code_mode::CodeModeToolSession::new(
            &[remote.clone()],
            host.clone(),
            crate::code_mode::CodeMode::Optional,
            &Default::default(),
        );
        let exec = cells
            .tools()
            .into_iter()
            .find(|tool| tool.name() == "exec")
            .unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            exec.call(
                json!({"source":r#"
try { await tools.mcp__fixture__pending({marker:"synthetic-private-input"}); }
catch (error) { text(JSON.parse(String(error))); }
"#}),
                &cx,
            ),
        )
        .await
        .unwrap();
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        assert!(content.contains("mcp_remote_outcome_unknown"), "{content}");
        assert!(content.contains("Do not retry"), "{content}");
        assert!(content.contains("remote_stop_confirmed"), "{content}");
        tokio::time::timeout(Duration::from_secs(1), fixture.cancelled.notified())
            .await
            .unwrap();
        let result = alias.call(json!({}), &cx).await;
        let (content, is_error) = result.into_content();
        assert!(is_error);
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["structuredContent"]["code"], "mcp_server_quarantined");
        assert_eq!(value["structuredContent"]["completion"], "not_started");
        assert_eq!(value["structuredContent"]["retry_safe"], false);
        let nested = host
            .call_tool(ToolInvocation {
                name: remote.name().into(),
                input_json: json!({}),
            })
            .await
            .unwrap();
        assert!(nested.is_error);
        assert!(nested.content.contains("prior_completion_unknown"));
        let marker = remote.uncertain_outcome().unwrap();
        assert!(marker.contains("deadline_exceeded"));
        assert!(!marker.contains("synthetic-private-input"));
        assert_eq!(alias.uncertain_outcome(), Some(marker));
        {
            let requests = fixture.requests.lock().unwrap();
            let calls: Vec<_> = requests
                .iter()
                .filter(|request| request["method"] == "tools/call")
                .collect();
            assert_eq!(calls.len(), 1, "quarantine must prevent subsequent RPCs");
            let notification = requests
                .iter()
                .find(|request| request["method"] == "notifications/cancelled")
                .unwrap();
            assert_eq!(notification["params"]["requestId"], calls[0]["id"]);
        }
        cells.shutdown().await.unwrap();
        close(fixture).await;
    }

    #[tokio::test]
    async fn cancellation_is_unknown_remote_completion_not_successful_cancellation() {
        let fixture = fixture(300_000, false).await;
        let connection = fixture.connection.clone();
        let cancellation = CancellationToken::new();
        let worker_cancel = cancellation.clone();
        let call = tokio::spawn(async move {
            connection
                .call_tool(CallToolRequestParams::new("pending"), &worker_cancel)
                .await
        });
        tokio::time::timeout(Duration::from_secs(1), fixture.called.notified())
            .await
            .unwrap();
        cancellation.cancel();
        let result = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .unwrap()
            .unwrap();
        let (content, is_error) = result.into_content();
        assert!(is_error);
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["structuredContent"]["completion"], "unknown");
        assert_eq!(value["structuredContent"]["reason"], "cancelled_wait");
        assert_eq!(value["structuredContent"]["remote_stop_confirmed"], false);
        assert_eq!(value["structuredContent"]["cancellation_requested"], true);
        tokio::time::timeout(Duration::from_secs(1), fixture.cancelled.notified())
            .await
            .unwrap();
        close(fixture).await;
    }

    #[tokio::test]
    async fn completed_remote_result_remains_actual_and_connection_stays_usable() {
        let fixture = fixture(1000, true).await;
        for _ in 0..2 {
            let result = fixture
                .connection
                .call_tool(
                    CallToolRequestParams::new("pending"),
                    &CancellationToken::new(),
                )
                .await;
            let (content, is_error) = result.into_content();
            assert!(!is_error, "{content}");
            assert!(content.contains("completed"));
            assert!(fixture.connection.uncertain_outcome().is_none());
        }
        assert_eq!(
            fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "tools/call")
                .count(),
            2
        );
        close(fixture).await;
    }

    #[tokio::test]
    async fn protocol_rejection_preserves_error_and_allows_another_request() {
        let fixture = fixture_with_error(1000, true, Some(-32602)).await;
        for _ in 0..2 {
            let result = fixture
                .connection
                .call_tool(
                    CallToolRequestParams::new("pending"),
                    &CancellationToken::new(),
                )
                .await;
            let (content, is_error) = result.into_content();
            assert!(is_error);
            let value: Value = serde_json::from_str(&content).unwrap();
            assert_eq!(value["structuredContent"]["code"], "mcp_protocol_rejection");
            assert_eq!(value["structuredContent"]["completion"], "rejected");
            assert_eq!(
                value["structuredContent"]["protocol_error"],
                json!({
                    "code":-32602, "message":"synthetic rejection", "data":{"field":"fixture"}
                })
            );
            assert_eq!(value["content"][0]["text"], "synthetic rejection");
            assert!(fixture.connection.uncertain_outcome().is_none());
        }
        assert_eq!(
            fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "tools/call")
                .count(),
            2
        );
        close(fixture).await;
    }

    #[tokio::test]
    async fn internal_protocol_error_preserves_evidence_but_quarantines_unknown_effects() {
        let fixture = fixture_with_error(1000, true, Some(-32603)).await;
        let result = fixture
            .connection
            .call_tool(
                CallToolRequestParams::new("pending"),
                &CancellationToken::new(),
            )
            .await;
        let (content, is_error) = result.into_content();
        assert!(is_error);
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(
            value["structuredContent"]["code"],
            "mcp_remote_outcome_unknown"
        );
        assert_eq!(value["structuredContent"]["completion"], "unknown");
        assert_eq!(
            value["structuredContent"]["protocol_error"],
            json!({
                "code":-32603, "message":"synthetic rejection", "data":{"field":"fixture"}
            })
        );
        assert!(fixture.connection.uncertain_outcome().is_some());
        let next = fixture
            .connection
            .call_tool(
                CallToolRequestParams::new("pending"),
                &CancellationToken::new(),
            )
            .await;
        let (content, is_error) = next.into_content();
        assert!(is_error);
        let value: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(value["structuredContent"]["completion"], "not_started");
        assert_eq!(
            fixture
                .requests
                .lock()
                .unwrap()
                .iter()
                .filter(|request| request["method"] == "tools/call")
                .count(),
            1
        );
        close(fixture).await;
    }

    #[tokio::test]
    async fn in_process_mutation_keeps_actual_owner_despite_remote_policy_and_cancel() {
        struct Local {
            started: Arc<Notify>,
            release: Arc<Notify>,
        }
        #[async_trait]
        impl McpSurface for Local {
            async fn list_tools(&self) -> anyhow::Result<Vec<McpToolSpec>> {
                Ok(vec![McpToolSpec {
                    name: "mutate".into(),
                    input_schema: json!({"type":"object"}),
                    ..Default::default()
                }])
            }
            async fn call_tool(&self, _: &str, _: Value) -> anyhow::Result<ToolResult> {
                self.started.notify_one();
                self.release.notified().await;
                Ok(ToolResult::Json(json!({"actual_completed":true})))
            }
        }
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let config = McpConfig {
            servers: vec![McpServerConfig::InProcess {
                name: "local".into(),
                server: Arc::new(Local {
                    started: started.clone(),
                    release: release.clone(),
                }),
            }],
            tool_placement: Default::default(),
            server_policies: BTreeMap::from([(
                "local".into(),
                McpServerPolicy {
                    tool_timeout_ms: 1,
                    ..Default::default()
                },
            )]),
        };
        let loaded = load_mcp_tools_from_config(&config, &ToolFilter::default())
            .await
            .unwrap();
        let tool = loaded.tools[0].clone();
        let directory = tempfile::tempdir().unwrap();
        let cx = cx(directory.path().canonicalize().unwrap());
        let cancellation = cx.cancellation.clone();
        let mut call = tokio::spawn(async move { tool.call(json!({}), &cx).await });
        tokio::time::timeout(Duration::from_secs(1), started.notified())
            .await
            .unwrap();
        cancellation.cancel();
        assert!(
            tokio::time::timeout(Duration::from_millis(10), &mut call)
                .await
                .is_err()
        );
        release.notify_one();
        let result = tokio::time::timeout(Duration::from_secs(1), call)
            .await
            .unwrap()
            .unwrap();
        let (content, is_error) = result.into_content();
        assert!(!is_error, "{content}");
        assert!(content.contains("actual_completed"));
        assert!(loaded.tools[0].uncertain_outcome().is_none());
    }
}

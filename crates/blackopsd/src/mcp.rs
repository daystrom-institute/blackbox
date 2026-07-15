use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use blackops_core::{
    ApprovalCreateRequest, ApprovalId, ApprovalResolveRequest, DefinitionInstallRequest,
    IntegrationIntentId, IntegrationIntentResolveRequest, InvocationId, InvocationRequest,
    ScheduleIntent, WaitCreateRequest, WaitId, WaitResolveRequest, WebhookAdmissionRequest,
    WhiteboardCreateRequest, WhiteboardId, WhiteboardPutRequest,
};
use bro_capabilities::{
    AgentCapability, AgentMessageRequest, AgentSpawnRequest, AgentTarget, AgentWaitRequest,
    AtomCapability, AtomInvocation,
};
use bro_core::SessionId;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::runtime::now_ms;
use crate::service::{AgentCall, AgentListCall, AppState};

const MAX_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct JsonRpcRequest {
    jsonrpc: String,
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct InvocationLookup {
    invocation_id: String,
}

#[derive(Debug, Deserialize)]
struct TriggerDue {
    #[serde(default)]
    now_unix_ms: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct WebhookCall {
    path: String,
    request: WebhookAdmissionRequest,
}

#[derive(Debug, Deserialize)]
struct WaitResolveCall {
    wait_id: String,
    request: WaitResolveRequest,
}

#[derive(Debug, Deserialize)]
struct ApprovalResolveCall {
    approval_id: String,
    request: ApprovalResolveRequest,
}

#[derive(Debug, Deserialize)]
struct IntegrationResolveCall {
    integration_intent_id: String,
    request: IntegrationIntentResolveRequest,
}

#[derive(Debug, Deserialize)]
struct WhiteboardLookup {
    whiteboard_id: String,
}

#[derive(Debug, Deserialize)]
struct WhiteboardPutCall {
    whiteboard_id: String,
    key: String,
    request: WhiteboardPutRequest,
}

fn empty_object() -> Value {
    json!({})
}

pub(crate) async fn handle(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<JsonRpcRequest>,
) -> Response {
    if !accepts_streamable_http(&headers) {
        return (
            StatusCode::NOT_ACCEPTABLE,
            "Accept must include application/json and text/event-stream",
        )
            .into_response();
    }
    let notification = request.id.is_none();
    let id = request.id.clone().unwrap_or(Value::Null);
    if request.jsonrpc != "2.0" {
        return Json(rpc_error(id, -32600, "jsonrpc must be 2.0")).into_response();
    }
    let result = match request.method.as_str() {
        "initialize" => Ok(json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {"tools": {"listChanged": false}},
            "serverInfo": {
                "name": "blackopsd",
                "version": env!("CARGO_PKG_VERSION"),
                "buildId": state.runtime.build_id()
            }
        })),
        "notifications/initialized" => Ok(json!({})),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tool_catalog()})),
        "tools/call" => match serde_json::from_value::<ToolCall>(request.params) {
            Ok(call) => {
                if serde_json::to_vec(&call.arguments)
                    .map_or(true, |bytes| bytes.len() > MAX_ARGUMENT_BYTES)
                {
                    Err("tool arguments exceed the 64 KiB operational bound".into())
                } else {
                    dispatch_tool(&state, call).await
                }
            }
            Err(error) => Err(format!("invalid tools/call parameters: {error}")),
        },
        method => {
            return Json(rpc_error(id, -32601, &format!("unknown method {method}")))
                .into_response();
        }
    };
    if notification {
        return StatusCode::ACCEPTED.into_response();
    }
    Json(match result {
        Ok(value) if request.method == "tools/call" => rpc_result(id, tool_result(value, false)),
        Ok(value) => rpc_result(id, value),
        Err(error) if request.method == "tools/call" => {
            rpc_result(id, tool_result(Value::String(error), true))
        }
        Err(error) => rpc_error(id, -32602, &error),
    })
    .into_response()
}

fn accepts_streamable_http(headers: &HeaderMap) -> bool {
    let accepts = headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .collect::<Vec<_>>()
        .join(",");
    accepts.contains("application/json") && accepts.contains("text/event-stream")
}

async fn dispatch_tool(state: &AppState, call: ToolCall) -> Result<Value, String> {
    match call.name.as_str() {
        "blackops_agent_spawn" => {
            let call: AgentCall<AgentSpawnRequest> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(capability.spawn(call.request).await.map_err(bro_error)?)
        }
        "blackops_agent_send" => {
            let call: AgentCall<AgentMessageRequest> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            capability
                .send_message(call.request)
                .await
                .map_err(bro_error)?;
            Ok(json!({"accepted": true}))
        }
        "blackops_agent_followup" => {
            let call: AgentCall<AgentMessageRequest> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            capability.followup(call.request).await.map_err(bro_error)?;
            Ok(json!({"accepted": true}))
        }
        "blackops_agent_interrupt" => {
            let call: AgentCall<AgentTarget> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(
                capability
                    .interrupt(call.request)
                    .await
                    .map_err(bro_error)?,
            )
        }
        "blackops_agent_status" => {
            let call: AgentCall<AgentTarget> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(capability.status(call.request).await.map_err(bro_error)?)
        }
        "blackops_agent_list" => {
            let call: AgentListCall = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(capability.list(call.prefix).await.map_err(bro_error)?)
        }
        "blackops_agent_wait" => {
            let call: AgentCall<AgentWaitRequest> = parse(call.arguments)?;
            let capability = state.runtime.session_agents(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(capability.wait(call.request).await.map_err(bro_error)?)
        }
        "blackops_atom_invoke" => {
            let call: AgentCall<AtomInvocation> = parse(call.arguments)?;
            let capability = state.runtime.session_atoms(
                call.worker_id,
                SessionId::new(call.session_id),
                call.call_id,
            );
            to_value(
                capability
                    .invoke_atom(call.request)
                    .await
                    .map_err(bro_error)?,
            )
        }
        "blackops_definition_install" => {
            let request: DefinitionInstallRequest = parse(call.arguments)?;
            let definition = state
                .runtime
                .authority()
                .call(move |authority| authority.install_definition(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(definition)
        }
        "blackops_definition_list" => {
            let definitions = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_definitions()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(definitions)
        }
        "blackops_invocation_request" => {
            let request: InvocationRequest = parse(call.arguments)?;
            let invocation = state
                .runtime
                .authority()
                .call(move |authority| authority.request_invocation(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(invocation)
        }
        "blackops_invocation_list" => {
            let invocations = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_invocations()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(invocations)
        }
        "blackops_invocation_status" => {
            let lookup: InvocationLookup = parse(call.arguments)?;
            let invocation_id = InvocationId::new(lookup.invocation_id);
            let invocation = state
                .runtime
                .authority()
                .call(move |authority| authority.invocation(&invocation_id))
                .await
                .map_err(|error| error.to_string())?;
            to_value(invocation)
        }
        "blackops_workflow_list" => {
            let workflows = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_workflow_runs()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(workflows)
        }
        "blackops_workflow_status" => {
            let lookup: InvocationLookup = parse(call.arguments)?;
            let invocation_id = InvocationId::new(lookup.invocation_id);
            let workflow = state
                .runtime
                .authority()
                .call(move |authority| authority.workflow_run(&invocation_id))
                .await
                .map_err(|error| error.to_string())?;
            to_value(workflow)
        }
        "blackops_integration_list" => {
            let intents = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_integration_intents()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(intents)
        }
        "blackops_integration_resolve" => {
            let call: IntegrationResolveCall = parse(call.arguments)?;
            let intent_id = IntegrationIntentId::new(call.integration_intent_id);
            let intent = state
                .runtime
                .authority()
                .call(move |authority| {
                    authority.resolve_integration_intent(&intent_id, call.request)
                })
                .await
                .map_err(|error| error.to_string())?;
            to_value(intent)
        }
        "blackops_schedule_put" => {
            let request: ScheduleIntent = parse(call.arguments)?;
            let schedule = state
                .runtime
                .authority()
                .call(move |authority| authority.put_schedule(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(schedule)
        }
        "blackops_schedule_list" => {
            let schedules = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_schedules()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(schedules)
        }
        "blackops_schedule_trigger_due" => {
            let request: TriggerDue = parse(call.arguments)?;
            let now = request.now_unix_ms.unwrap_or_else(now_ms);
            let limit = request.limit.unwrap_or(64);
            let invocations = state
                .runtime
                .authority()
                .call(move |authority| authority.trigger_due_schedules(now, limit))
                .await
                .map_err(|error| error.to_string())?;
            to_value(invocations)
        }
        "blackops_schedule_admit_webhook" => {
            let call: WebhookCall = parse(call.arguments)?;
            let invocations = state
                .runtime
                .authority()
                .call(move |authority| authority.admit_webhook(&call.path, call.request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(invocations)
        }
        "blackops_wait_create" => {
            let request: WaitCreateRequest = parse(call.arguments)?;
            let wait = state
                .runtime
                .authority()
                .call(move |authority| authority.create_wait(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(wait)
        }
        "blackops_wait_list" => {
            let waits = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_waits()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(waits)
        }
        "blackops_wait_resolve" => {
            let call: WaitResolveCall = parse(call.arguments)?;
            let wait_id = WaitId::new(call.wait_id);
            let wait = state
                .runtime
                .authority()
                .call(move |authority| authority.resolve_wait(&wait_id, call.request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(wait)
        }
        "blackops_approval_request" => {
            let request: ApprovalCreateRequest = parse(call.arguments)?;
            let approval = state
                .runtime
                .authority()
                .call(move |authority| authority.request_approval(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(approval)
        }
        "blackops_approval_list" => {
            let approvals = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_approvals()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(approvals)
        }
        "blackops_approval_resolve" => {
            let call: ApprovalResolveCall = parse(call.arguments)?;
            let approval_id = ApprovalId::new(call.approval_id);
            let approval = state
                .runtime
                .authority()
                .call(move |authority| authority.resolve_approval(&approval_id, call.request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(approval)
        }
        "blackops_whiteboard_create" => {
            let request: WhiteboardCreateRequest = parse(call.arguments)?;
            let board = state
                .runtime
                .authority()
                .call(move |authority| authority.create_whiteboard(request))
                .await
                .map_err(|error| error.to_string())?;
            to_value(board)
        }
        "blackops_whiteboard_list" => {
            let boards = state
                .runtime
                .authority()
                .call(|authority| Ok(authority.list_whiteboards()))
                .await
                .map_err(|error| error.to_string())?;
            to_value(boards)
        }
        "blackops_whiteboard_get" => {
            let call: WhiteboardLookup = parse(call.arguments)?;
            let board_id = WhiteboardId::new(call.whiteboard_id);
            let board = state
                .runtime
                .authority()
                .call(move |authority| authority.whiteboard(&board_id))
                .await
                .map_err(|error| error.to_string())?;
            to_value(board)
        }
        "blackops_whiteboard_put" => {
            let call: WhiteboardPutCall = parse(call.arguments)?;
            let board_id = WhiteboardId::new(call.whiteboard_id);
            let board = state
                .runtime
                .authority()
                .call(move |authority| {
                    authority.put_whiteboard_entry(&board_id, &call.key, call.request)
                })
                .await
                .map_err(|error| error.to_string())?;
            to_value(board)
        }
        name => Err(format!("unknown operational tool {name}")),
    }
}

fn tool_catalog() -> Vec<Value> {
    [
        ("blackops_agent_spawn", "Spawn a logical child agent", agent_schema(true)),
        ("blackops_agent_send", "Append a mailbox message without waking the target", agent_schema(true)),
        ("blackops_agent_followup", "Append a message and wake the target", agent_schema(true)),
        ("blackops_agent_interrupt", "Request interruption of a logical agent", agent_schema(true)),
        ("blackops_agent_status", "Read logical agent status", agent_schema(true)),
        ("blackops_agent_list", "List agents within the caller's authorized tree", agent_list_schema()),
        ("blackops_agent_wait", "Wait for bounded logical-agent activity", agent_schema(true)),
        ("blackops_atom_invoke", "Invoke an exact immutable atom definition", agent_schema(true)),
        ("blackops_definition_install", "Install an immutable versioned operational definition", object_schema()),
        ("blackops_definition_list", "List operational definitions", object_schema()),
        ("blackops_invocation_request", "Create an idempotent definition invocation intent", object_schema()),
        ("blackops_invocation_list", "List definition invocations", object_schema()),
        ("blackops_invocation_status", "Read one definition invocation", required_string_schema("invocation_id")),
        ("blackops_workflow_list", "List durable workflow runs", closed_object_schema()),
        ("blackops_workflow_status", "Read one durable workflow run", required_string_schema("invocation_id")),
        ("blackops_integration_list", "List publish and integration intents", closed_object_schema()),
        ("blackops_integration_resolve", "Resolve a publish or integration intent", integration_resolve_schema()),
        ("blackops_schedule_put", "Create or update a durable schedule", object_schema()),
        ("blackops_schedule_list", "List durable schedules", object_schema()),
        ("blackops_schedule_trigger_due", "Materialize bounded due schedule invocations", object_schema()),
        ("blackops_schedule_admit_webhook", "Admit an idempotent webhook delivery", webhook_schema()),
        ("blackops_wait_create", "Create a durable wait", wait_create_schema()),
        ("blackops_wait_list", "List durable waits", closed_object_schema()),
        ("blackops_wait_resolve", "Resolve or cancel a durable wait", wait_resolve_schema()),
        ("blackops_approval_request", "Request a durable operator approval", approval_create_schema()),
        ("blackops_approval_list", "List durable approvals", closed_object_schema()),
        ("blackops_approval_resolve", "Resolve a durable approval", approval_resolve_schema()),
        ("blackops_whiteboard_create", "Create a durable shared whiteboard", whiteboard_create_schema()),
        ("blackops_whiteboard_list", "List durable shared whiteboards", closed_object_schema()),
        ("blackops_whiteboard_get", "Read a durable shared whiteboard", required_string_schema("whiteboard_id")),
        ("blackops_whiteboard_put", "Compare-and-swap a whiteboard entry", whiteboard_put_schema()),
    ]
    .into_iter()
    .map(|(name, description, input_schema)| {
        json!({"name": name, "description": description, "inputSchema": input_schema})
    })
    .collect()
}

fn object_schema() -> Value {
    json!({"type": "object", "additionalProperties": true})
}

fn closed_object_schema() -> Value {
    json!({"type": "object", "additionalProperties": false})
}

fn wait_create_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "wait_id": {"type": "string"},
            "topic": {"type": "string"},
            "selector": {},
            "deadline_unix_ms": {"type": ["integer", "null"], "minimum": 0},
            "created_at_unix_ms": {"type": "integer", "minimum": 0}
        },
        "required": ["wait_id", "topic", "selector", "created_at_unix_ms"],
        "additionalProperties": false
    })
}

fn approval_create_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "approval_id": {"type": "string"},
            "requested_by": {"type": "string"},
            "action": {},
            "created_at_unix_ms": {"type": "integer", "minimum": 0}
        },
        "required": ["approval_id", "requested_by", "action", "created_at_unix_ms"],
        "additionalProperties": false
    })
}

fn whiteboard_create_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "whiteboard_id": {"type": "string"},
            "name": {"type": "string"},
            "created_at_unix_ms": {"type": "integer", "minimum": 0}
        },
        "required": ["whiteboard_id", "name", "created_at_unix_ms"],
        "additionalProperties": false
    })
}

fn wait_resolve_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "wait_id": {"type": "string"},
            "request": {
                "type": "object",
                "properties": {
                    "status": {"enum": ["satisfied", "cancelled", "timed_out"]},
                    "resolution": {},
                    "resolved_at_unix_ms": {"type": "integer", "minimum": 0}
                },
                "required": ["status", "resolution", "resolved_at_unix_ms"],
                "additionalProperties": false
            }
        },
        "required": ["wait_id", "request"],
        "additionalProperties": false
    })
}

fn approval_resolve_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "approval_id": {"type": "string"},
            "request": {
                "type": "object",
                "properties": {
                    "status": {"enum": ["approved", "rejected", "cancelled"]},
                    "decision": {},
                    "resolved_at_unix_ms": {"type": "integer", "minimum": 0}
                },
                "required": ["status", "decision", "resolved_at_unix_ms"],
                "additionalProperties": false
            }
        },
        "required": ["approval_id", "request"],
        "additionalProperties": false
    })
}

fn integration_resolve_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "integration_intent_id": {"type": "string"},
            "request": {
                "type": "object",
                "properties": {
                    "status": {"enum": ["completed", "failed"]},
                    "result": {},
                    "resolved_at_unix_ms": {"type": "integer", "minimum": 0}
                },
                "required": ["status", "result", "resolved_at_unix_ms"],
                "additionalProperties": false
            }
        },
        "required": ["integration_intent_id", "request"],
        "additionalProperties": false
    })
}

fn whiteboard_put_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "whiteboard_id": {"type": "string"},
            "key": {"type": "string", "minLength": 1, "maxLength": 256},
            "request": {
                "type": "object",
                "properties": {
                    "value": {},
                    "expected_revision": {"type": ["integer", "null"], "minimum": 0},
                    "updated_by": {"type": "string"},
                    "updated_at_unix_ms": {"type": "integer", "minimum": 0}
                },
                "required": ["value", "expected_revision", "updated_by", "updated_at_unix_ms"],
                "additionalProperties": false
            }
        },
        "required": ["whiteboard_id", "key", "request"],
        "additionalProperties": false
    })
}

fn required_string_schema(name: &str) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(name.to_string(), json!({"type": "string"}));
    json!({
        "type": "object",
        "properties": properties,
        "required": [name],
        "additionalProperties": false
    })
}

fn agent_schema(request_required: bool) -> Value {
    let mut required = vec!["call_id", "worker_id", "session_id"];
    if request_required {
        required.push("request");
    }
    json!({
        "type": "object",
        "properties": {
            "call_id": {"type": "string"},
            "worker_id": {"type": "string"},
            "session_id": {"type": "string"},
            "request": {"type": "object"}
        },
        "required": required,
        "additionalProperties": false
    })
}

fn agent_list_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "call_id": {"type": "string"},
            "worker_id": {"type": "string"},
            "session_id": {"type": "string"},
            "prefix": {"type": ["string", "null"]}
        },
        "required": ["call_id", "worker_id", "session_id"],
        "additionalProperties": false
    })
}

fn webhook_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "path": {"type": "string"},
            "request": {"type": "object"}
        },
        "required": ["path", "request"],
        "additionalProperties": false
    })
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("invalid tool arguments: {error}"))
}

fn to_value(value: impl serde::Serialize) -> Result<Value, String> {
    serde_json::to_value(value).map_err(|error| error.to_string())
}

fn bro_error(error: bro_core::BroError) -> String {
    format!("{}: {}", error.code, error.message)
}

fn tool_result(value: Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(text) if is_error => text,
        value => serde_json::to_string(&value).unwrap_or_else(|_| "null".into()),
    };
    json!({
        "content": [{"type": "text", "text": text}],
        "isError": is_error
    })
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {"code": code, "message": message}
    })
}

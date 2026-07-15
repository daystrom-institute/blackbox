use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bro_protocol::CloseoutRequest;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::control::{
    ControlState, DashboardParams, TaskOnly, TaskPrompt, WaitParams, cancel_value, closeout_value,
    dashboard_value, exec_value, interrupt_value, resume_value, roster_value, status_value,
    steer_value, wait_value,
};

pub(crate) const MAX_HTTP_BODY_BYTES: usize = 128 * 1024;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;
const DEFAULT_TOOL_DEADLINE: Duration = Duration::from_secs(60);
const WAIT_TOOL_DEADLINE: Duration = Duration::from_secs(905);
const CLOSEOUT_TOOL_DEADLINE: Duration = Duration::from_secs(1_800);

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
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_object")]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StatusParams {
    task_id: String,
    #[serde(default, rename = "tail")]
    _tail: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

fn empty_object() -> Value {
    json!({})
}

pub(crate) async fn handle(
    State(state): State<ControlState>,
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
                "name": "fleetd",
                "version": env!("CARGO_PKG_VERSION"),
                "buildId": crate::roster::fleet_build_id()
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
                    Err("tool arguments exceed the 64 KiB fleet bound".into())
                } else {
                    let deadline = match call.name.as_str() {
                        "bro_wait" => WAIT_TOOL_DEADLINE,
                        "bro_closeout" => CLOSEOUT_TOOL_DEADLINE,
                        _ => DEFAULT_TOOL_DEADLINE,
                    };
                    match tokio::time::timeout(deadline, dispatch_tool(&state, call)).await {
                        Ok(result) => result,
                        Err(_) => Err(format!(
                            "fleet tool call exceeded its {} second deadline",
                            deadline.as_secs()
                        )),
                    }
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

async fn dispatch_tool(state: &ControlState, call: ToolCall) -> Result<Value, String> {
    let result = match call.name.as_str() {
        "bro_exec" => exec_value(state, parse(call.arguments)?).await,
        "bro_resume" => resume_value(state, parse(call.arguments)?).await,
        "bro_status" => {
            let input: StatusParams = parse(call.arguments)?;
            status_value(state, &input.task_id)
        }
        "bro_wait" => wait_value(state, parse::<WaitParams>(call.arguments)?).await,
        "bro_steer" => steer_value(state, parse::<TaskPrompt>(call.arguments)?).await,
        "bro_interrupt" => interrupt_value(state, parse::<TaskOnly>(call.arguments)?).await,
        "bro_cancel" => cancel_value(state, parse::<TaskOnly>(call.arguments)?).await,
        "bro_roster" => {
            let _: EmptyParams = parse(call.arguments)?;
            roster_value(state)
        }
        "bro_dashboard" => dashboard_value(state, parse::<DashboardParams>(call.arguments)?),
        "bro_closeout" => {
            let request: CloseoutRequest = parse(call.arguments)?;
            closeout_value(state, request)
                .await
                .and_then(|outcome| serde_json::to_value(outcome).map_err(Into::into))
        }
        name => return Err(format!("unknown fleet tool {name}")),
    };
    result.map_err(|error| error.to_string())
}

fn tool_catalog() -> Vec<Value> {
    [
        (
            "bro_exec",
            "Start one idempotent fleet execution attempt",
            exec_schema(false),
        ),
        (
            "bro_resume",
            "Resume a durable provider session as a new fleet attempt",
            exec_schema(true),
        ),
        (
            "bro_status",
            "Read one task from fleetd's materialized roster",
            status_schema(),
        ),
        (
            "bro_wait",
            "Wait for one task with a bounded timeout; timeout returns a snapshot",
            wait_schema(),
        ),
        (
            "bro_steer",
            "Queue a user steer into a live fleet worker",
            task_prompt_schema(),
        ),
        (
            "bro_interrupt",
            "Interrupt the current turn of a live fleet worker",
            task_only_schema(),
        ),
        (
            "bro_cancel",
            "Persist an idempotent graceful cancellation request",
            task_only_schema(),
        ),
        (
            "bro_roster",
            "Read the complete materialized fleet roster snapshot",
            closed_object_schema(),
        ),
        (
            "bro_dashboard",
            "List bounded recent fleet tasks with provider and status filters",
            dashboard_schema(),
        ),
        (
            "bro_closeout",
            "Run the fleet-owned phased managed-worktree closeout driver",
            closeout_schema(),
        ),
    ]
    .into_iter()
    .map(|(name, description, input_schema)| {
        json!({"name": name, "description": description, "inputSchema": input_schema})
    })
    .collect()
}

fn exec_schema(resume: bool) -> Value {
    let mut required = vec!["provider", "prompt"];
    if resume {
        required.push("session_id");
    }
    json!({
        "type": "object",
        "properties": {
            "provider": {"type": "string"},
            "prompt": {"type": "string", "minLength": 1},
            "session_id": {"type": "string"},
            "cwd": {"type": "string"},
            "pin_model": {"type": "string"},
            "pin_effort": {"type": "string"},
            "code_mode": {"enum": ["off", "optional", "only"]},
            "service_tier": {"enum": ["default", "priority", "flex"]},
            "display_name": {"type": "string"},
            "idempotency_key": {"type": "string"},
            "allow_recursion": {"type": "boolean"},
            "allowed_remote_operations": {
                "type": "object",
                "additionalProperties": {
                    "type": "array",
                    "items": {"type": "string"},
                    "uniqueItems": true
                }
            },
            "allowed_atom_refs": {
                "type": "array",
                "items": {"type": "string", "pattern": "^atom:[^@*\\s]+@[^*\\s]+$"},
                "uniqueItems": true
            }
        },
        "required": required,
        "additionalProperties": false
    })
}

fn status_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string"},
            "tail": {"type": "integer", "minimum": 0}
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}

fn wait_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string"},
            "timeout_seconds": {"type": "number", "minimum": 0, "maximum": 900}
        },
        "required": ["task_id"],
        "additionalProperties": false
    })
}

fn task_prompt_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "task_id": {"type": "string"},
            "prompt": {"type": "string", "minLength": 1}
        },
        "required": ["task_id", "prompt"],
        "additionalProperties": false
    })
}

fn task_only_schema() -> Value {
    json!({
        "type": "object",
        "properties": {"task_id": {"type": "string"}},
        "required": ["task_id"],
        "additionalProperties": false
    })
}

fn dashboard_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "provider": {"type": "string"},
            "status": {"enum": ["pending", "running", "completed", "failed", "cancelled"]},
            "limit": {"type": "integer", "minimum": 0, "maximum": 200},
            "tail": {"type": "integer", "minimum": 0}
        },
        "additionalProperties": false
    })
}

fn closeout_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "worktree": {"type": "string"},
            "disposition": {"enum": ["keep", "preflight", "discard", "publish", "merge", "adopt"]},
            "confirm": {"type": "boolean"},
            "target": {"type": ["string", "null"]},
            "commit_message": {"type": ["string", "null"]},
            "paths": {"type": "array", "items": {"type": "string"}},
            "allow_branch_prefixes": {
                "type": ["array", "null"],
                "items": {"type": "string"}
            },
            "dry_run": {"type": "boolean"},
            "closeout_hooks": {"type": ["object", "null"]}
        },
        "required": ["worktree", "disposition", "confirm"],
        "additionalProperties": false
    })
}

fn closed_object_schema() -> Value {
    json!({"type": "object", "additionalProperties": false})
}

fn parse<T: DeserializeOwned>(value: Value) -> Result<T, String> {
    serde_json::from_value(value).map_err(|error| format!("invalid tool arguments: {error}"))
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

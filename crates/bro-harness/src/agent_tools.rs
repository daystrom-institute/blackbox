//! Provider-neutral model tools over one session-bound agent capability.

use std::sync::Arc;

use async_trait::async_trait;
use bro_capabilities::{
    AgentCapability, AgentForkTurns, AgentMessageRequest, AgentSpawnRequest, AgentTarget,
    AgentWaitRequest,
};
use bro_tools::{Tool, ToolCx, ToolResult};
use serde_json::{Value, json};

pub(crate) fn tools(capability: Arc<dyn AgentCapability>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(SpawnAgent(capability.clone())),
        Arc::new(SendMessage(capability.clone())),
        Arc::new(FollowupTask(capability.clone())),
        Arc::new(InterruptAgent(capability.clone())),
        Arc::new(ListAgents(capability.clone())),
        Arc::new(WaitAgent(capability)),
    ]
}

fn capability_error(operation: &str, error: bro_core::BroError) -> ToolResult {
    ToolResult::Error(format!(
        "{operation} failed: {}: {}",
        error.code, error.message
    ))
}

fn invocation_id(cx: &ToolCx) -> Result<&str, ToolResult> {
    cx.invocation_id().ok_or_else(|| {
        ToolResult::Error(
            "agent capability call is missing its stable tool invocation identity".into(),
        )
    })
}

fn required_string(input: &Value, key: &str) -> Result<String, ToolResult> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| ToolResult::Error(format!("`{key}` is required")))
}

fn message_request(input: &Value) -> Result<AgentMessageRequest, ToolResult> {
    Ok(AgentMessageRequest {
        target: AgentTarget {
            canonical_path: required_string(input, "target")?,
        },
        message: required_string(input, "message")?,
    })
}

struct SpawnAgent(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for SpawnAgent {
    fn name(&self) -> &str {
        "spawn_agent"
    }

    fn description(&self) -> &str {
        "Create a logical child agent below this session and request its first execution attempt."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task_name": {
                    "type": "string",
                    "pattern": "^[a-z0-9_]+$",
                    "description": "Lowercase child name using letters, digits, and underscores."
                },
                "message": {"type": "string"},
                "fork_turns": {
                    "description": "History fork: none, all, or a positive turn count.",
                    "oneOf": [
                        {"type": "string", "enum": ["none", "all"]},
                        {"type": "integer", "minimum": 1}
                    ]
                }
            },
            "required": ["task_name", "message"],
            "additionalProperties": false
        })
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let task_name = match required_string(&input, "task_name") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let message = match required_string(&input, "message") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let fork_turns = match input.get("fork_turns") {
            None => AgentForkTurns::All,
            Some(Value::String(value)) if value == "all" => AgentForkTurns::All,
            Some(Value::String(value)) if value == "none" => AgentForkTurns::None,
            Some(Value::Number(value)) => {
                match value.as_u64().and_then(|n| u32::try_from(n).ok()) {
                    Some(turns) if turns > 0 => AgentForkTurns::Recent(turns),
                    _ => {
                        return ToolResult::Error("`fork_turns` must be a positive integer".into());
                    }
                }
            }
            _ => {
                return ToolResult::Error(
                    "`fork_turns` must be `none`, `all`, or a positive integer".into(),
                );
            }
        };
        let invocation_id = match invocation_id(cx) {
            Ok(invocation_id) => invocation_id,
            Err(error) => return error,
        };
        match self
            .0
            .spawn_for_invocation(
                invocation_id,
                AgentSpawnRequest {
                    task_name,
                    message,
                    fork_turns,
                },
            )
            .await
        {
            Ok(identity) => ToolResult::Json(json!({
                "agent_id": identity.agent_id,
                "canonical_path": identity.canonical_path,
            })),
            Err(error) => capability_error("spawn_agent", error),
        }
    }
}

struct SendMessage(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for SendMessage {
    fn name(&self) -> &str {
        "send_message"
    }

    fn description(&self) -> &str {
        "Queue a message for an addressable agent without starting a new turn."
    }

    fn input_schema(&self) -> Value {
        message_schema()
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let request = match message_request(&input) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let invocation_id = match invocation_id(cx) {
            Ok(invocation_id) => invocation_id,
            Err(error) => return error,
        };
        match self
            .0
            .send_message_for_invocation(invocation_id, request)
            .await
        {
            Ok(()) => ToolResult::Json(json!({"queued": true, "triggered": false})),
            Err(error) => capability_error("send_message", error),
        }
    }
}

struct FollowupTask(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for FollowupTask {
    fn name(&self) -> &str {
        "followup_task"
    }

    fn description(&self) -> &str {
        "Queue follow-up work and trigger the target when it is idle."
    }

    fn input_schema(&self) -> Value {
        message_schema()
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let request = match message_request(&input) {
            Ok(request) => request,
            Err(error) => return error,
        };
        let invocation_id = match invocation_id(cx) {
            Ok(invocation_id) => invocation_id,
            Err(error) => return error,
        };
        match self.0.followup_for_invocation(invocation_id, request).await {
            Ok(()) => ToolResult::Json(json!({"queued": true, "triggered": true})),
            Err(error) => capability_error("followup_task", error),
        }
    }
}

fn message_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "description": "Canonical agent path."},
            "message": {"type": "string"}
        },
        "required": ["target", "message"],
        "additionalProperties": false
    })
}

struct InterruptAgent(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for InterruptAgent {
    fn name(&self) -> &str {
        "interrupt_agent"
    }

    fn description(&self) -> &str {
        "Interrupt the target's current turn while preserving its logical identity."
    }

    fn input_schema(&self) -> Value {
        target_schema()
    }

    async fn call(&self, input: Value, cx: &ToolCx) -> ToolResult {
        let target = match required_string(&input, "target") {
            Ok(canonical_path) => AgentTarget { canonical_path },
            Err(error) => return error,
        };
        let invocation_id = match invocation_id(cx) {
            Ok(invocation_id) => invocation_id,
            Err(error) => return error,
        };
        match self.0.interrupt_for_invocation(invocation_id, target).await {
            Ok(status) => ToolResult::Json(json!({"status": status})),
            Err(error) => capability_error("interrupt_agent", error),
        }
    }
}

fn target_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": {"type": "string", "description": "Canonical agent path."}
        },
        "required": ["target"],
        "additionalProperties": false
    })
}

struct ListAgents(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for ListAgents {
    fn name(&self) -> &str {
        "list_agents"
    }

    fn description(&self) -> &str {
        "List addressable agents and lifecycle status without exposing mailbox payloads."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path_prefix": {"type": "string"}
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let prefix = input
            .get("path_prefix")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.0.list(prefix).await {
            Ok(agents) => ToolResult::Json(json!({"agents": agents})),
            Err(error) => capability_error("list_agents", error),
        }
    }
}

struct WaitAgent(Arc<dyn AgentCapability>);

#[async_trait]
impl Tool for WaitAgent {
    fn name(&self) -> &str {
        "wait_agent"
    }

    fn description(&self) -> &str {
        "Wait for mailbox or descendant status changes and return only the typed wake reason."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 3600000},
                "path_prefix": {"type": "string"},
                "after_mailbox_sequence": {"type": "integer", "minimum": 0}
            },
            "additionalProperties": false
        })
    }

    async fn call(&self, input: Value, _cx: &ToolCx) -> ToolResult {
        let request = AgentWaitRequest {
            timeout_ms: input.get("timeout_ms").and_then(Value::as_u64),
            path_prefix: input
                .get("path_prefix")
                .and_then(Value::as_str)
                .map(str::to_string),
            after_mailbox_sequence: input.get("after_mailbox_sequence").and_then(Value::as_u64),
        };
        match self.0.wait(request).await {
            Ok(wake) => ToolResult::Json(json!({"wake": wake})),
            Err(error) => capability_error("wait_agent", error),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use bro_capabilities::{AgentIdentity, AgentStatus, AgentSummary, AgentWake};
    use bro_core::AgentId;

    use super::*;

    fn test_cx() -> ToolCx {
        ToolCx {
            invocation_id: Some(Arc::from("test-agent-tool-call")),
            root: std::env::temp_dir(),
            safety: Arc::new(bro_tools::SafetyPolicy::new()),
            http: reqwest::Client::new(),
            todos: Arc::new(Mutex::new(bro_tools::TodoList::default())),
            shell_sessions: Arc::new(Mutex::new(bro_tools::ShellSessions::default())),
            edits: Arc::new(Mutex::new(bro_tools::EditSink::default())),
            session_env: Arc::new(std::collections::BTreeMap::new()),
            tool_arg_defaults: Arc::new(bro_tools::ToolArgDefaults::default()),
            shell_env: Arc::new(Default::default()),
        }
    }

    #[derive(Default)]
    struct FakeAgent {
        calls: Mutex<Vec<String>>,
    }

    #[async_trait]
    impl AgentCapability for FakeAgent {
        async fn spawn(
            &self,
            request: AgentSpawnRequest,
        ) -> Result<AgentIdentity, bro_core::BroError> {
            self.calls.lock().unwrap().push(request.task_name.clone());
            Ok(AgentIdentity {
                agent_id: AgentId::new("agent-1"),
                canonical_path: format!("/root/{}", request.task_name),
            })
        }

        async fn send_message(
            &self,
            _request: AgentMessageRequest,
        ) -> Result<(), bro_core::BroError> {
            Ok(())
        }

        async fn followup(&self, _request: AgentMessageRequest) -> Result<(), bro_core::BroError> {
            Ok(())
        }

        async fn interrupt(&self, _target: AgentTarget) -> Result<AgentStatus, bro_core::BroError> {
            Ok(AgentStatus::Interrupted)
        }

        async fn status(&self, target: AgentTarget) -> Result<AgentSummary, bro_core::BroError> {
            Ok(AgentSummary {
                identity: AgentIdentity {
                    agent_id: AgentId::new("agent-1"),
                    canonical_path: target.canonical_path,
                },
                status: AgentStatus::Idle,
                last_attempt_id: None,
                unavailable_cause: None,
            })
        }

        async fn list(
            &self,
            _prefix: Option<String>,
        ) -> Result<Vec<AgentSummary>, bro_core::BroError> {
            Ok(Vec::new())
        }

        async fn wait(&self, _request: AgentWaitRequest) -> Result<AgentWake, bro_core::BroError> {
            Ok(AgentWake::Timeout)
        }
    }

    #[tokio::test]
    async fn spawn_tool_maps_model_schema_to_typed_capability() {
        let fake = Arc::new(FakeAgent::default());
        let tool = SpawnAgent(fake.clone());
        let result = tool
            .call(
                json!({"task_name": "reviewer", "message": "review", "fork_turns": 2}),
                &test_cx(),
            )
            .await;
        assert!(matches!(result, ToolResult::Json(_)));
        assert_eq!(fake.calls.lock().unwrap().as_slice(), ["reviewer"]);
    }
}

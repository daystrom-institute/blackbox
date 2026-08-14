use std::sync::Arc;

use serde_json::{Value, json};

use crate::server::state::SharedState;
use crate::system_events::gate;
use crate::system_events::hub::{EventHub, SystemEventDraft};
use crate::system_events::template::UnresolvedPolicy;
use crate::system_events::types::*;

const SUMMARY_CAP: usize = 4096;

pub(super) async fn execute_action(
    state: Arc<SharedState>,
    hub: &EventHub,
    reaction: &ReactionSpec,
    event: &SystemEvent,
) -> ActionOutcome {
    let roots = build_event_roots(event);
    match &reaction.action {
        ReactionAction::EmitEvent { args } => exec_emit_event(hub, event, args, &roots).await,
        ReactionAction::HttpJson { args } => exec_http_json(args, &roots).await,
        ReactionAction::McpCall { args } => exec_mcp_call(args, &roots, event).await,
        ReactionAction::AtomInvoke { args } => exec_atom_invoke(args, &roots, event, &state).await,
        ReactionAction::StartWorkflow { args } => {
            exec_start_workflow(args, &roots, event, state).await
        }
    }
}

async fn exec_emit_event(
    hub: &EventHub,
    event: &SystemEvent,
    args: &Value,
    roots: &serde_json::Map<String, Value>,
) -> ActionOutcome {
    let rendered = match render_args(args, roots) {
        Ok(v) => v,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("template render: {e}")}),
            };
        }
    };
    let kind_str = match rendered["kind"].as_str() {
        Some(s) => s,
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": "emit_event args missing 'kind'"}),
            };
        }
    };
    let draft = SystemEventDraft {
        kind: SystemEventKind::from_wire(kind_str),
        producer: "reaction:emit_event".to_string(),
        project: event.project.clone(),
        principal: None,
        subject: None,
        correlation: serde_json::Map::new(),
        causation_id: Some(event.id.clone()),
        payload: rendered.get("payload").cloned().unwrap_or(json!({})),
    };
    match hub.emit(draft).await {
        Ok(outcome) => ActionOutcome {
            status: ActionStatus::Succeeded,
            response_summary: json!({
                "emitted_event_id": outcome.event.id,
                "kind": outcome.event.kind.to_wire(),
            }),
        },
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.contains("causation depth") {
                ActionOutcome {
                    status: ActionStatus::PermanentFailure,
                    response_summary: redact_summary(json!({"error": msg})),
                }
            } else {
                ActionOutcome {
                    status: ActionStatus::RetryableFailure,
                    response_summary: redact_summary(json!({"error": msg})),
                }
            }
        }
    }
}

async fn exec_http_json(args: &Value, roots: &serde_json::Map<String, Value>) -> ActionOutcome {
    let rendered = match render_args(args, roots) {
        Ok(v) => v,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("template render: {e}")}),
            };
        }
    };
    let spec = match crate::orchestration::http_fetch::HttpFetchSpec::from_args(&rendered) {
        Ok(s) => s,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("http_json spec: {e}")}),
            };
        }
    };
    let expect_set: Vec<u16> = spec.expect_status.clone().unwrap_or_default();
    match spec.execute().await {
        Ok(result) => {
            let mut summary = json!({
                "status": result.status,
                "response_kind": format!("{:?}", spec.response_kind),
            });
            if let Value::Object(map) = &mut summary {
                let body_preview = truncate_str(
                    &serde_json::to_string(&result.value).unwrap_or_default(),
                    2048,
                );
                map.insert("body_preview".to_string(), Value::String(body_preview));
            }
            ActionOutcome {
                status: ActionStatus::Succeeded,
                response_summary: redact_summary(summary),
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let status = classify_http_error(&msg, &expect_set);
            ActionOutcome {
                status,
                response_summary: redact_summary(json!({"error": truncate_str(&msg, SUMMARY_CAP)})),
            }
        }
    }
}

async fn exec_mcp_call(
    args: &Value,
    roots: &serde_json::Map<String, Value>,
    event: &SystemEvent,
) -> ActionOutcome {
    let rendered = match render_args(args, roots) {
        Ok(v) => v,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("template render: {e}")}),
            };
        }
    };
    let server = match rendered["server"].as_str() {
        Some(s) => s.to_string(),
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": "mcp_call requires args.server"}),
            };
        }
    };
    let tool = match rendered["tool"].as_str() {
        Some(s) => s.to_string(),
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": "mcp_call requires args.tool"}),
            };
        }
    };
    let arguments = match rendered["arguments"].as_object() {
        Some(map) => map.clone(),
        None => serde_json::Map::new(),
    };
    let timeout_secs = cap_mcp_timeout(rendered["timeout_secs"].as_u64());
    let project_dir = event.project.as_deref();
    let cwd = rendered["cwd"].as_str().map(|s| s.to_string());

    match crate::mcp_client::call_tool(
        &server,
        &tool,
        arguments,
        timeout_secs,
        project_dir,
        cwd.as_deref(),
    )
    .await
    {
        Ok(result) => {
            let truncated = truncate_json(&result, SUMMARY_CAP);
            ActionOutcome {
                status: ActionStatus::Succeeded,
                response_summary: redact_summary(json!({
                    "server": server,
                    "tool": tool,
                    "result_preview": truncated,
                })),
            }
        }
        Err(e) => {
            let msg = format!("{e:#}");
            let retryable = msg.contains("timed out") || msg.contains("connection refused");
            ActionOutcome {
                status: if retryable {
                    ActionStatus::RetryableFailure
                } else {
                    ActionStatus::PermanentFailure
                },
                response_summary: redact_summary(json!({
                    "server": server,
                    "tool": tool,
                    "error": truncate_str(&msg, SUMMARY_CAP),
                })),
            }
        }
    }
}

async fn exec_atom_invoke(
    args: &Value,
    roots: &serde_json::Map<String, Value>,
    event: &SystemEvent,
    state: &Arc<SharedState>,
) -> ActionOutcome {
    let rendered = match render_args(args, roots) {
        Ok(v) => v,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("template render: {e}")}),
            };
        }
    };
    let atom = match rendered["atom"].as_str() {
        Some(s) => s.to_string(),
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": "atom_invoke requires args.atom"}),
            };
        }
    };
    let atom_args = rendered.get("args").cloned().unwrap_or(json!({}));

    if super::forgejo::is_builtin_atom(&atom) {
        return super::forgejo::run_forgejo_ensure_user(&atom_args, event, state).await;
    }

    let project_dir = match rendered.get("project_dir").and_then(|v| v.as_str()) {
        Some(s) => Some(s.to_string()),
        None => event.project.clone(),
    };

    let params = crate::tools::bro_params::AtomInvokeParams {
        atom: atom.clone(),
        args: atom_args,
        project_dir: project_dir.clone(),
        owner: Some(format!("reaction:{}", event.id)),
        parent_invocation_id: None,
        runtime: None,
        supervision_override: None,
        suppress_auto_supervision: false,
    };

    let server = crate::server::BlackboxServer::new(state.clone());
    match server.atom_invoke_value(params, None).await {
        Ok(val) => {
            let handle = val
                .get("invocation_id")
                .or_else(|| val.get("handle"))
                .cloned()
                .unwrap_or_else(|| Value::String(format!("inv-{}", uuid::Uuid::new_v4())));
            ActionOutcome {
                status: ActionStatus::Succeeded,
                response_summary: redact_summary(json!({
                    "atom": atom,
                    "invocation_id": handle,
                })),
            }
        }
        Err(e) => ActionOutcome {
            status: ActionStatus::PermanentFailure,
            response_summary: redact_summary(json!({
                "atom": atom,
                "error": truncate_str(&e, SUMMARY_CAP),
            })),
        },
    }
}

async fn exec_start_workflow(
    args: &Value,
    roots: &serde_json::Map<String, Value>,
    event: &SystemEvent,
    state: Arc<SharedState>,
) -> ActionOutcome {
    let rendered = match render_args(args, roots) {
        Ok(v) => v,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": format!("template render: {e}")}),
            };
        }
    };
    let workflow_id = match rendered["workflow"].as_str() {
        Some(s) => s.to_string(),
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({"error": "start_workflow requires args.workflow"}),
            };
        }
    };
    let project_dir = match rendered.get("project_dir").and_then(|v| v.as_str()) {
        Some(s) => Some(s.to_string()),
        None => event.project.clone(),
    };

    let initial_vars = rendered
        .get("initial_vars")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();

    let registry = state.workflow_registry.read();
    let workflow_def = match registry.get(&workflow_id) {
        Some(w) => w.clone(),
        None => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({
                    "error": format!("workflow '{}' not found in registry", workflow_id),
                }),
            };
        }
    };
    drop(registry);

    let compiled = match crate::workflow::compile(workflow_def) {
        Ok(c) => c,
        Err(e) => {
            return ActionOutcome {
                status: ActionStatus::PermanentFailure,
                response_summary: json!({
                    "error": format!("workflow compile: {e:#}"),
                }),
            };
        }
    };

    let arc_id = format!("arc-{}", uuid::Uuid::new_v4().simple());

    tokio::spawn({
        let pd = project_dir.clone();
        let owned_arc_id = arc_id.clone();
        async move {
            let server = crate::server::BlackboxServer::new(state);
            let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
            crate::workflow::engine::run_workflow_streaming_with_vars_and_arc_id(
                &server,
                &compiled,
                pd,
                Some(50),
                initial_vars,
                event_tx,
                owned_arc_id,
            )
            .await;
        }
    });

    ActionOutcome {
        status: ActionStatus::Succeeded,
        response_summary: redact_summary(json!({
            "workflow": workflow_id,
            "arc_id": arc_id,
            "project_dir": project_dir,
        })),
    }
}

fn build_event_roots(event: &SystemEvent) -> serde_json::Map<String, Value> {
    let mut roots = serde_json::Map::new();
    roots.insert(
        "event".to_string(),
        serde_json::to_value(event).unwrap_or_default(),
    );
    roots
}

fn render_args(
    args: &Value,
    roots: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, anyhow::Error> {
    render_value(args, roots)
}

fn render_value(
    val: &Value,
    roots: &serde_json::Map<String, Value>,
) -> std::result::Result<Value, anyhow::Error> {
    match val {
        Value::String(s) => {
            let rendered = crate::system_events::template::render_template_with_roots(
                s,
                roots,
                UnresolvedPolicy::HardError,
            )?;
            Ok(Value::String(rendered))
        }
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(k.clone(), render_value(v, roots)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(items) => {
            let rendered: std::result::Result<Vec<_>, _> =
                items.iter().map(|i| render_value(i, roots)).collect();
            Ok(Value::Array(rendered?))
        }
        other => Ok(other.clone()),
    }
}

fn classify_http_error(msg: &str, expect_set: &[u16]) -> ActionStatus {
    if let Some(status) = extract_http_status(msg) {
        if status == 429 || (500..600).contains(&status) {
            return ActionStatus::RetryableFailure;
        }
        if !expect_set.is_empty() && expect_set.contains(&status) {
            return ActionStatus::RetryableFailure;
        }
        if (400..500).contains(&status) {
            return ActionStatus::PermanentFailure;
        }
    }
    if msg.contains("timed out")
        || msg.contains("connect")
        || msg.contains("dns")
        || msg.contains("connection refused")
    {
        return ActionStatus::RetryableFailure;
    }
    ActionStatus::PermanentFailure
}

fn cap_mcp_timeout(requested: Option<u64>) -> u64 {
    requested.unwrap_or(300).min(600)
}

fn extract_http_status(msg: &str) -> Option<u16> {
    for part in msg.split_whitespace() {
        if let Ok(code) = part.trim_end_matches(':').parse::<u16>() {
            if (100..600).contains(&code) {
                return Some(code);
            }
        }
    }
    None
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        let mut end = max;
        while !s.is_char_boundary(end) && end > 0 {
            end -= 1;
        }
        format!("{}...[truncated]", &s[..end])
    }
}

fn truncate_json(v: &Value, max: usize) -> Value {
    let s = serde_json::to_string(v).unwrap_or_default();
    Value::String(truncate_str(&s, max))
}

fn redact_summary(v: Value) -> Value {
    gate::redact_values(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event() -> SystemEvent {
        SystemEvent {
            id: "evt-test-001".to_string(),
            kind: SystemEventKind::from_wire("test.event"),
            producer: "test".to_string(),
            project: Some("/test/project".to_string()),
            principal: None,
            subject: None,
            correlation: serde_json::Map::new(),
            causation_id: None,
            payload: json!({}),
            occurred_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    fn make_roots(event: &SystemEvent) -> serde_json::Map<String, Value> {
        let mut roots = serde_json::Map::new();
        roots.insert(
            "event".to_string(),
            serde_json::to_value(event).unwrap_or_default(),
        );
        roots
    }

    fn test_state() -> (Arc<SharedState>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let state = SharedState::for_test(dir.path());
        (Arc::new(state), dir)
    }

    async fn spawn_json_handler(
        status_code: u16,
        body: Value,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let app = axum::Router::new().route(
            "/test",
            axum::routing::post(move || {
                let code = status_code;
                let body = body.clone();
                async move {
                    let s = axum::http::StatusCode::from_u16(code)
                        .unwrap_or(axum::http::StatusCode::INTERNAL_SERVER_ERROR);
                    (s, axum::Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let port = addr.port();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://127.0.0.1:{port}/test"), handle)
    }

    #[tokio::test]
    async fn http_json_success_200() {
        let (url, _handle) = spawn_json_handler(200, json!({"ok": true})).await;
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"url": url, "method": "POST", "body": {"test": 1}});

        let outcome = exec_http_json(&args, &roots).await;
        assert_eq!(outcome.status, ActionStatus::Succeeded);
        assert_eq!(outcome.response_summary["status"].as_u64(), Some(200));
        assert!(
            outcome.response_summary["body_preview"]
                .as_str()
                .unwrap()
                .contains("ok")
        );
    }

    #[tokio::test]
    async fn http_json_500_retryable() {
        let (url, _handle) = spawn_json_handler(500, json!({"error": "internal"})).await;
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"url": url, "method": "POST"});

        let outcome = exec_http_json(&args, &roots).await;
        assert_eq!(outcome.status, ActionStatus::RetryableFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(
            err.contains("500"),
            "error should mention status 500: {err}"
        );
    }

    #[tokio::test]
    async fn http_json_400_permanent() {
        let (url, _handle) = spawn_json_handler(400, json!({"error": "bad request"})).await;
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"url": url, "method": "POST"});

        let outcome = exec_http_json(&args, &roots).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(
            err.contains("400"),
            "error should mention status 400: {err}"
        );
    }

    #[tokio::test]
    async fn http_json_redacts_authorization_in_error_summary() {
        let event = make_event();
        let roots = make_roots(&event);
        let (url, _handle) =
            spawn_json_handler(400, json!({"error": "secret:my-api-key-123"})).await;
        let args = json!({
            "url": url,
            "headers": {
                "Authorization": "Bearer secret:my-api-key-123"
            }
        });
        let outcome = exec_http_json(&args, &roots).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let summary_str = serde_json::to_string(&outcome.response_summary).unwrap();
        assert!(
            !summary_str.contains("secret:my-api-key-123"),
            "response_summary should not contain secret ref: {summary_str}"
        );
        assert!(
            !summary_str.contains("Bearer secret:my-api-key-123"),
            "response_summary should not contain raw bearer value: {summary_str}"
        );
    }

    #[tokio::test]
    async fn mcp_call_missing_server_permanent() {
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"tool": "some_tool"});

        let outcome = exec_mcp_call(&args, &roots, &event).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(err.contains("server"), "error should mention server: {err}");
    }

    #[tokio::test]
    async fn mcp_call_missing_tool_permanent() {
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"server": "some_server"});

        let outcome = exec_mcp_call(&args, &roots, &event).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(err.contains("tool"), "error should mention tool: {err}");
    }

    #[tokio::test]
    async fn atom_invoke_missing_atom_permanent() {
        let (state, _dir) = test_state();
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"args": {"x": 1}});

        let outcome = exec_atom_invoke(&args, &roots, &event, &state).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(err.contains("atom"), "error should mention atom: {err}");
    }

    #[tokio::test]
    async fn start_workflow_missing_workflow_permanent() {
        let (state, _dir) = test_state();
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"workflow": "nonexistent"});

        let outcome = exec_start_workflow(&args, &roots, &event, state).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        let err = outcome.response_summary["error"].as_str().unwrap();
        assert!(
            err.contains("not found"),
            "error should mention not found: {err}"
        );
    }

    #[test]
    fn classify_http_429_retryable() {
        assert_eq!(
            classify_http_error("HTTP 429: rate limited", &[]),
            ActionStatus::RetryableFailure
        );
    }

    #[test]
    fn classify_http_500_retryable() {
        assert_eq!(
            classify_http_error("HTTP 500: internal", &[]),
            ActionStatus::RetryableFailure
        );
    }

    #[test]
    fn classify_http_400_permanent() {
        assert_eq!(
            classify_http_error("HTTP 400: bad request", &[]),
            ActionStatus::PermanentFailure
        );
    }

    #[test]
    fn classify_http_404_permanent() {
        assert_eq!(
            classify_http_error("HTTP 404: not found", &[]),
            ActionStatus::PermanentFailure
        );
    }

    #[test]
    fn classify_network_retryable() {
        assert_eq!(
            classify_http_error("connection refused", &[]),
            ActionStatus::RetryableFailure
        );
    }

    #[test]
    fn classify_timeout_retryable() {
        assert_eq!(
            classify_http_error("timed out after 30s", &[]),
            ActionStatus::RetryableFailure
        );
    }

    #[test]
    fn truncate_str_short() {
        assert_eq!(truncate_str("hello", 10), "hello");
    }

    #[test]
    fn truncate_str_long() {
        let long = "a".repeat(5000);
        let out = truncate_str(&long, 100);
        assert!(out.len() < 200);
        assert!(out.ends_with("...[truncated]"));
    }

    #[test]
    fn extract_http_status_from_message() {
        assert_eq!(extract_http_status("HTTP 500: error"), Some(500));
        assert_eq!(extract_http_status("HTTP 404:"), Some(404));
        assert_eq!(extract_http_status("no status here"), None);
    }

    #[test]
    fn cap_mcp_timeout_defaults_and_caps() {
        assert_eq!(cap_mcp_timeout(None), 300);
        assert_eq!(cap_mcp_timeout(Some(100)), 100);
        assert_eq!(cap_mcp_timeout(Some(600)), 600);
        assert_eq!(cap_mcp_timeout(Some(9999)), 600);
        assert_eq!(cap_mcp_timeout(Some(1)), 1);
    }

    fn install_minimal_workflow(state: &Arc<SharedState>, name: &str) {
        use crate::workflow::schema::{
            ActorKind, ActorSpec, NodeMode, NodeSpec, NodeTransition, Workflow,
        };
        let wf = Workflow {
            name: name.to_string(),
            version: 1,
            actors: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "noop".to_string(),
                    ActorSpec {
                        kind: ActorKind::Executor,
                        brofile: None,
                        team: None,
                        durable: false,
                        compaction_anchor: false,
                        requires: Vec::new(),
                        runtime: None,
                        timeout: None,
                    },
                );
                m
            },
            atom_bindings: std::collections::HashMap::new(),
            nodes: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "start".to_string(),
                    NodeSpec {
                        actor: String::new(),
                        atom: String::new(),
                        atom_args: None,
                        prompt: Some("noop".to_string()),
                        gate: None,
                        gate_mode: None,
                        mode: NodeMode::Sync,
                        retry: None,
                        late_inject: None,
                        subworkflow: None,
                        subworkflow_ref: None,
                        imports: Vec::new(),
                        exports: Vec::new(),
                        import_renames: std::collections::HashMap::new(),
                        on_enter: Vec::new(),
                        on_exit: Vec::new(),
                        wait: None,
                        sleep: None,
                        wait_for: Vec::new(),
                        foreach: None,
                        matrix: None,
                        board: None,
                        timeout: None,
                        actor_failure: None,
                        next: NodeTransition::Terminal,
                    },
                );
                m
            },
            start: "start".to_string(),
            policy_packet: None,
            vars_schema: None,
            on_arc_exit: Vec::new(),
            on_arc_cancel: Vec::new(),
            shell_allowlist: None,
        };
        state.workflow_registry.write().insert(name.to_string(), wf);
    }

    #[tokio::test]
    async fn start_workflow_smoke_succeeds_with_real_workflow() {
        let (state, _dir) = test_state();
        install_minimal_workflow(&state, "smoke-wf");

        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"workflow": "smoke-wf"});

        let outcome = exec_start_workflow(&args, &roots, &event, state).await;
        assert_eq!(outcome.status, ActionStatus::Succeeded);
        let arc_id = outcome.response_summary["arc_id"].as_str().unwrap();
        assert!(
            arc_id.starts_with("arc-"),
            "arc_id should be minted: {arc_id}"
        );
        let hex_part = &arc_id[4..];
        assert_eq!(hex_part.len(), 32, "arc_id should be `arc-` + 32 chars");
        assert!(
            hex_part.chars().all(|c| c.is_ascii_hexdigit()),
            "arc_id should look like arc-<32 hex>: {arc_id}"
        );
        assert_eq!(
            arc_id.len(),
            36,
            "engine-style arc ids use `arc-` + 32 hex chars"
        );
        assert_eq!(
            outcome.response_summary["workflow"].as_str(),
            Some("smoke-wf")
        );
    }

    async fn install_echo_atom(state: &Arc<SharedState>, name: &str) {
        use crate::artifacts::{ArtifactInstallParams, ArtifactKind};
        use crate::server::routes::install_artifact_value;
        let atom_json = json!({
            "_contract": "atom/v1",
            "kind": "atom",
            "name": name,
            "version": 1,
            "manifest": {
                "description": "Echo deterministic atom for executor tests.",
                "when_to_use": ["when testing atom_invoke executor"],
                "inputs": {
                    "schema": {"type": "object", "additionalProperties": true}
                },
                "outputs": {
                    "schema": {
                        "type": "object",
                        "required": ["echo"],
                        "properties": {"echo": {}}
                    }
                },
                "effects": {
                    "writes_files": false,
                    "dispatches_runs": 0,
                    "max_depth": 0,
                    "uses_network": false
                },
                "composition": {"may_invoke_atoms": {"kind": "none"}},
                "implementation": {"kind": "deterministic", "runner": "echo"}
            }
        });
        install_artifact_value(
            state,
            ArtifactInstallParams {
                kind: ArtifactKind::Atom,
                source: format!("{name}.json"),
                name: None,
                version: None,
                supersedes: None,
            },
            atom_json,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn atom_invoke_echo_succeeds() {
        let (state, _dir) = test_state();
        install_echo_atom(&state, "test-echo").await;

        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({
            "atom": "atom:test-echo@v1",
            "args": {"message": "hello from executor test"}
        });

        let outcome = exec_atom_invoke(&args, &roots, &event, &state).await;
        assert_eq!(outcome.status, ActionStatus::Succeeded);
        let inv_id = outcome.response_summary["invocation_id"].as_str().unwrap();
        assert!(!inv_id.is_empty(), "should have invocation_id");
        assert_eq!(
            outcome.response_summary["atom"].as_str(),
            Some("atom:test-echo@v1")
        );
    }

    #[tokio::test]
    async fn atom_invoke_unknown_atom_permanent_failure() {
        let (state, _dir) = test_state();
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({"atom": "atom:nonexistent@v1", "args": {}});

        let outcome = exec_atom_invoke(&args, &roots, &event, &state).await;
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);
        assert!(
            outcome.response_summary["error"]
                .as_str()
                .unwrap()
                .contains("nonexistent"),
            "error should mention the atom name"
        );
    }

    /// Verify that exec_atom_invoke routes forgejo-ensure-user@v1 to the
    /// built-in forgejo path rather than falling through to the atom registry.
    /// When the admin token secret is absent the forgejo path returns a
    /// PermanentFailure whose error mentions "admin token" or "resolving" —
    /// not the generic "not found" / "not installed" that the atom registry
    /// would produce for an unregistered atom.
    #[tokio::test]
    async fn atom_invoke_forgejo_builtin_routes_to_forgejo_path() {
        let (state, _dir) = test_state();
        let event = make_event();
        let roots = make_roots(&event);
        let args = json!({
            "atom": "atom:forgejo-ensure-user@v1",
            "args": {
                "instance": "test",
                "bro": "test-bro",
                "provider": "claude",
                "model": "haiku-4.5",
                "username": "bro-test",
                "display_name": "test bro",
                "email": "test@test.local",
                "token_secret_name": "forgejo-bro-test-routing-check",
                "admin_token_ref": "secret:forgejo-admin-token-routing-check",
                "base_url": "http://127.0.0.1:1"
            }
        });

        let outcome = exec_atom_invoke(&args, &roots, &event, &state).await;
        // Should always be PermanentFailure (admin token not found OR
        // connection refused is retryable, but missing secret is permanent)
        assert_eq!(outcome.status, ActionStatus::PermanentFailure);

        let err = outcome.response_summary["error"].as_str().unwrap();
        // The forgejo path prefixes missing-config errors with "permanent:"
        // followed by the anyhow context chain. Must NOT say "not found" or
        // "not installed" (which would indicate the atom registry path).
        assert!(
            !err.contains("not found") && !err.contains("not installed"),
            "error should come from forgejo path, not atom registry: {err}"
        );
        // The forgejo path generates either "admin token" or "required arg"
        // type permanent errors when secrets are absent.
        assert!(
            err.contains("permanent")
                || err.contains("token")
                || err.contains("resolving")
                || err.contains("forgejo"),
            "error should be from the forgejo path: {err}"
        );
    }
}

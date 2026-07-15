use axum::extract::{DefaultBodyLimit, Path, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use blackops_core::{
    ApprovalCreateRequest, ApprovalId, ApprovalResolveRequest, DefinitionInstallRequest,
    IntegrationIntentId, IntegrationIntentResolveRequest, InvocationId, InvocationRequest,
    ScheduleIntent, SendMessageRequest, TeamAssignment, TeamCreateRequest, WaitCreateRequest,
    WaitId, WaitResolveRequest, WebhookAdmissionRequest, WhiteboardCreateRequest, WhiteboardId,
    WhiteboardPutRequest,
};
use bro_capabilities::{
    AgentCapability, AgentMessageRequest, AgentSpawnRequest, AgentTarget, AgentWaitRequest,
    AtomCapability, AtomInvocation,
};
use bro_core::{BroError, OperationId, SessionId, WorkerId};
use bro_protocol::{
    CapabilityAuthorization, CapabilityError, CapabilityErrorCode, CapabilityRequest,
    CapabilityResponse,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

use crate::runtime::{SessionAttemptBinding, now_ms};
use crate::{BlackopsRuntime, BlackopsdError, BlackopsdResult};

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) runtime: BlackopsRuntime,
    pub(crate) service_token: Arc<bro_rpc::ServiceToken>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RoutedCapabilityRequest {
    pub worker_id: String,
    pub session_id: String,
    pub authorization: CapabilityAuthorization,
    pub request: CapabilityRequest,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentCall<T> {
    pub call_id: String,
    pub worker_id: String,
    pub session_id: String,
    pub request: T,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentListCall {
    pub call_id: String,
    pub worker_id: String,
    pub session_id: String,
    pub prefix: Option<String>,
}

pub fn router(runtime: BlackopsRuntime, service_token: Arc<bro_rpc::ServiceToken>) -> Router {
    Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/v1/status", get(status))
        .route("/v1/operations/{operation_id}", get(operation))
        .route("/v1/operations/reconcile", post(reconcile))
        .route("/v1/agents/spawn", post(agent_spawn))
        .route("/v1/agents/send", post(agent_send))
        .route("/v1/agents/followup", post(agent_followup))
        .route("/v1/agents/interrupt", post(agent_interrupt))
        .route("/v1/agents/status", post(agent_status))
        .route("/v1/agents/list", post(agent_list))
        .route("/v1/agents/wait", post(agent_wait))
        .route("/v1/teams", get(teams).post(create_team))
        .route("/v1/teams/assign", post(assign_team))
        .route("/v1/definitions", get(definitions).post(install_definition))
        .route("/v1/invocations", get(invocations).post(request_invocation))
        .route("/v1/invocations/{invocation_id}", get(invocation))
        .route("/v1/workflows", get(workflow_runs))
        .route("/v1/workflows/{invocation_id}", get(workflow_run))
        .route("/v1/integration-intents", get(integration_intents))
        .route(
            "/v1/integration-intents/{intent_id}/resolve",
            post(resolve_integration_intent),
        )
        .route("/v1/schedules", get(schedules).post(put_schedule))
        .route("/v1/schedules/trigger-due", post(trigger_due_schedules))
        .route("/v1/triggers/webhook/{*path}", post(admit_webhook))
        .route("/v1/waits", get(waits).post(create_wait))
        .route("/v1/waits/{wait_id}/resolve", post(resolve_wait))
        .route("/v1/approvals", get(approvals).post(request_approval))
        .route(
            "/v1/approvals/{approval_id}/resolve",
            post(resolve_approval),
        )
        .route("/v1/whiteboards", get(whiteboards).post(create_whiteboard))
        .route("/v1/whiteboards/{whiteboard_id}", get(whiteboard))
        .route(
            "/v1/whiteboards/{whiteboard_id}/entries/{key}",
            post(put_whiteboard_entry),
        )
        .route(
            "/mcp",
            post(crate::mcp::handle).layer(DefaultBodyLimit::max(128 * 1024)),
        )
        .route("/internal/capability", post(internal_capability))
        .route("/control/dashboard", get(compat_dashboard))
        .route("/control/team/{team}", get(compat_team))
        .route("/control/broadcast", post(compat_broadcast))
        .route_layer(middleware::from_fn_with_state(
            AppState {
                runtime: runtime.clone(),
                service_token: service_token.clone(),
            },
            require_service_auth,
        ))
        .with_state(AppState {
            runtime,
            service_token,
        })
}

async fn require_service_auth(
    State(state): State<AppState>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    if matches!(request.uri().path(), "/healthz" | "/readyz")
        || state.service_token.authorizes(request.headers())
    {
        next.run(request).await
    } else {
        (
            axum::http::StatusCode::UNAUTHORIZED,
            "bearer token required",
        )
            .into_response()
    }
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "blackopsd",
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": state.runtime.build_id()
    }))
}

async fn ready(State(state): State<AppState>) -> BlackopsdResult<Json<Value>> {
    let status = state.runtime.status().await?;
    Ok(Json(json!({
        "ready": true,
        "service": "blackopsd",
        "build_id": status.build_id,
        "generation": status.generation
    })))
}

async fn status(State(state): State<AppState>) -> BlackopsdResult<Json<crate::RuntimeStatus>> {
    Ok(Json(state.runtime.status().await?))
}

async fn operation(
    State(state): State<AppState>,
    Path(operation_id): Path<String>,
) -> BlackopsdResult<Json<blackops_core::OperationRecord>> {
    let operation_id = OperationId::new(operation_id);
    let operation = state
        .runtime
        .authority()
        .call(move |authority| authority.operation(&operation_id))
        .await?;
    Ok(Json(operation))
}

async fn reconcile(State(state): State<AppState>) -> Json<crate::ReconcileReport> {
    Json(state.runtime.drive_once().await)
}

async fn agent_spawn(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentSpawnRequest>>,
) -> BlackopsdResult<Json<bro_capabilities::AgentIdentity>> {
    let capability = session_capability(&state.runtime, &call);
    Ok(Json(capability.spawn(call.request).await?))
}

async fn agent_send(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentMessageRequest>>,
) -> BlackopsdResult<Json<Value>> {
    let capability = session_capability(&state.runtime, &call);
    capability.send_message(call.request).await?;
    Ok(Json(json!({"accepted": true})))
}

async fn agent_followup(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentMessageRequest>>,
) -> BlackopsdResult<Json<Value>> {
    let capability = session_capability(&state.runtime, &call);
    capability.followup(call.request).await?;
    Ok(Json(json!({"accepted": true})))
}

async fn agent_interrupt(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentTarget>>,
) -> BlackopsdResult<Json<bro_capabilities::AgentStatus>> {
    let capability = session_capability(&state.runtime, &call);
    Ok(Json(capability.interrupt(call.request).await?))
}

async fn agent_status(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentTarget>>,
) -> BlackopsdResult<Json<bro_capabilities::AgentSummary>> {
    let capability = session_capability(&state.runtime, &call);
    Ok(Json(capability.status(call.request).await?))
}

async fn agent_list(
    State(state): State<AppState>,
    Json(call): Json<AgentListCall>,
) -> BlackopsdResult<Json<Vec<bro_capabilities::AgentSummary>>> {
    let capability = state.runtime.session_agents(
        call.worker_id,
        SessionId::new(call.session_id),
        call.call_id,
    );
    Ok(Json(capability.list(call.prefix).await?))
}

async fn agent_wait(
    State(state): State<AppState>,
    Json(call): Json<AgentCall<AgentWaitRequest>>,
) -> BlackopsdResult<Json<bro_capabilities::AgentWake>> {
    let capability = session_capability(&state.runtime, &call);
    Ok(Json(capability.wait(call.request).await?))
}

async fn teams(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::TeamRecord>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_teams()))
            .await?,
    ))
}

async fn create_team(
    State(state): State<AppState>,
    Json(request): Json<TeamCreateRequest>,
) -> BlackopsdResult<Json<blackops_core::TeamRecord>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.create_team(request))
            .await?,
    ))
}

async fn assign_team(
    State(state): State<AppState>,
    Json(request): Json<TeamAssignment>,
) -> BlackopsdResult<Json<blackops_core::TeamRecord>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.assign_team_member(request))
            .await?,
    ))
}

async fn definitions(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::OperationalDefinition>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_definitions()))
            .await?,
    ))
}

async fn install_definition(
    State(state): State<AppState>,
    Json(request): Json<DefinitionInstallRequest>,
) -> BlackopsdResult<Json<blackops_core::OperationalDefinition>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.install_definition(request))
            .await?,
    ))
}

async fn request_invocation(
    State(state): State<AppState>,
    Json(request): Json<InvocationRequest>,
) -> BlackopsdResult<Json<blackops_core::InvocationIntent>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.request_invocation(request))
            .await?,
    ))
}

async fn invocations(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::InvocationIntent>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_invocations()))
            .await?,
    ))
}

async fn invocation(
    State(state): State<AppState>,
    Path(invocation_id): Path<String>,
) -> BlackopsdResult<Json<blackops_core::InvocationIntent>> {
    let invocation_id = InvocationId::new(invocation_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.invocation(&invocation_id))
            .await?,
    ))
}

async fn workflow_runs(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::WorkflowRun>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_workflow_runs()))
            .await?,
    ))
}

async fn workflow_run(
    State(state): State<AppState>,
    Path(invocation_id): Path<String>,
) -> BlackopsdResult<Json<blackops_core::WorkflowRun>> {
    let invocation_id = InvocationId::new(invocation_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.workflow_run(&invocation_id))
            .await?,
    ))
}

async fn integration_intents(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::IntegrationIntent>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_integration_intents()))
            .await?,
    ))
}

async fn resolve_integration_intent(
    State(state): State<AppState>,
    Path(intent_id): Path<String>,
    Json(request): Json<IntegrationIntentResolveRequest>,
) -> BlackopsdResult<Json<blackops_core::IntegrationIntent>> {
    let intent_id = IntegrationIntentId::new(intent_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.resolve_integration_intent(&intent_id, request))
            .await?,
    ))
}

async fn schedules(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::ScheduleIntent>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_schedules()))
            .await?,
    ))
}

async fn put_schedule(
    State(state): State<AppState>,
    Json(request): Json<ScheduleIntent>,
) -> BlackopsdResult<Json<ScheduleIntent>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.put_schedule(request))
            .await?,
    ))
}

#[derive(Debug, Deserialize)]
struct TriggerDueRequest {
    #[serde(default)]
    now_unix_ms: Option<u64>,
    #[serde(default)]
    limit: Option<usize>,
}

async fn trigger_due_schedules(
    State(state): State<AppState>,
    Json(request): Json<TriggerDueRequest>,
) -> BlackopsdResult<Json<Vec<blackops_core::InvocationIntent>>> {
    let now = request.now_unix_ms.unwrap_or_else(now_ms);
    let limit = request.limit.unwrap_or(64);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.trigger_due_schedules(now, limit))
            .await?,
    ))
}

async fn admit_webhook(
    State(state): State<AppState>,
    Path(path): Path<String>,
    Json(request): Json<WebhookAdmissionRequest>,
) -> BlackopsdResult<Json<Vec<blackops_core::InvocationIntent>>> {
    let path = format!("/{path}");
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.admit_webhook(&path, request))
            .await?,
    ))
}

async fn waits(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::OperationalWait>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_waits()))
            .await?,
    ))
}

async fn create_wait(
    State(state): State<AppState>,
    Json(request): Json<WaitCreateRequest>,
) -> BlackopsdResult<Json<blackops_core::OperationalWait>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.create_wait(request))
            .await?,
    ))
}

async fn resolve_wait(
    State(state): State<AppState>,
    Path(wait_id): Path<String>,
    Json(request): Json<WaitResolveRequest>,
) -> BlackopsdResult<Json<blackops_core::OperationalWait>> {
    let wait_id = WaitId::new(wait_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.resolve_wait(&wait_id, request))
            .await?,
    ))
}

async fn approvals(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::ApprovalRecord>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_approvals()))
            .await?,
    ))
}

async fn request_approval(
    State(state): State<AppState>,
    Json(request): Json<ApprovalCreateRequest>,
) -> BlackopsdResult<Json<blackops_core::ApprovalRecord>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.request_approval(request))
            .await?,
    ))
}

async fn resolve_approval(
    State(state): State<AppState>,
    Path(approval_id): Path<String>,
    Json(request): Json<ApprovalResolveRequest>,
) -> BlackopsdResult<Json<blackops_core::ApprovalRecord>> {
    let approval_id = ApprovalId::new(approval_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.resolve_approval(&approval_id, request))
            .await?,
    ))
}

async fn whiteboards(
    State(state): State<AppState>,
) -> BlackopsdResult<Json<Vec<blackops_core::WhiteboardRecord>>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(|authority| Ok(authority.list_whiteboards()))
            .await?,
    ))
}

async fn create_whiteboard(
    State(state): State<AppState>,
    Json(request): Json<WhiteboardCreateRequest>,
) -> BlackopsdResult<Json<blackops_core::WhiteboardRecord>> {
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.create_whiteboard(request))
            .await?,
    ))
}

async fn whiteboard(
    State(state): State<AppState>,
    Path(whiteboard_id): Path<String>,
) -> BlackopsdResult<Json<blackops_core::WhiteboardRecord>> {
    let whiteboard_id = WhiteboardId::new(whiteboard_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.whiteboard(&whiteboard_id))
            .await?,
    ))
}

async fn put_whiteboard_entry(
    State(state): State<AppState>,
    Path((whiteboard_id, key)): Path<(String, String)>,
    Json(request): Json<WhiteboardPutRequest>,
) -> BlackopsdResult<Json<blackops_core::WhiteboardRecord>> {
    let whiteboard_id = WhiteboardId::new(whiteboard_id);
    Ok(Json(
        state
            .runtime
            .authority()
            .call(move |authority| authority.put_whiteboard_entry(&whiteboard_id, &key, request))
            .await?,
    ))
}

async fn internal_capability(
    State(state): State<AppState>,
    Json(routed): Json<RoutedCapabilityRequest>,
) -> Json<CapabilityResponse> {
    let call_id = routed.request.call_id.clone();
    let worker_id = WorkerId::new(routed.worker_id.clone());
    let session_id = SessionId::new(routed.session_id.clone());
    if !routed.authorization.authorizes(
        &worker_id,
        &session_id,
        &routed.request.capability,
        &routed.request.operation,
    ) {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::Unauthorized,
            "fleet authorization does not grant this exact capability operation",
            false,
        ));
    }
    if routed.request.capability != "blackops.agent" && routed.request.capability != "atom" {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::Unauthorized,
            "blackopsd only serves blackops.agent and atom on this endpoint",
            false,
        ));
    }
    if routed
        .request
        .deadline_unix_ms
        .is_some_and(|deadline| now_ms() > deadline)
    {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::DeadlineExceeded,
            "capability deadline elapsed before dispatch",
            true,
        ));
    }
    // `call_id` correlates one RPC attempt and is deliberately not durable
    // effect identity. A retry after an ambiguous disconnect may use a new
    // call ID, so every mutating operation must carry the stable provider/tool
    // invocation identity. Read-only calls may use the correlation ID because
    // they cannot mint an operation, child, message, or atom invocation.
    let requires_invocation_id = routed.request.capability == "atom"
        || matches!(
            routed.request.operation.as_str(),
            "spawn" | "send_message" | "followup" | "interrupt"
        );
    let invocation_id = routed
        .request
        .invocation_id
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    if requires_invocation_id && invocation_id.is_none() {
        return Json(capability_error(
            call_id,
            CapabilityErrorCode::InvalidRequest,
            "mutating capability operation requires a stable invocation_id",
            false,
        ));
    }
    let invocation_id = invocation_id.unwrap_or_else(|| call_id.clone());
    if routed.request.capability == "atom" {
        if routed.request.operation != "invoke_atom" {
            return Json(capability_error(
                call_id,
                CapabilityErrorCode::InvalidRequest,
                "atom capability only supports invoke_atom",
                false,
            ));
        }
        let invocation =
            match serde_json::from_value::<AtomInvocation>(routed.request.bounded_payload) {
                Ok(invocation) => invocation,
                Err(error) => {
                    return Json(capability_error(
                        call_id,
                        CapabilityErrorCode::InvalidRequest,
                        &error.to_string(),
                        false,
                    ));
                }
            };
        if !routed
            .authorization
            .capability_policy
            .allows_atom_ref(&invocation.atom)
        {
            return Json(capability_error(
                call_id,
                CapabilityErrorCode::Unauthorized,
                "fleet authorization does not grant this exact atom ref",
                false,
            ));
        }
        let capability = state.runtime.session_atoms_until_with_policy(
            routed.worker_id,
            session_id,
            SessionAttemptBinding::authenticated(
                routed.authorization.task_id,
                routed.authorization.attempt_id,
                routed.authorization.session_attempt_generation,
            ),
            invocation_id,
            routed.request.deadline_unix_ms,
            routed.authorization.capability_policy,
        );
        return Json(match capability.invoke_atom(invocation).await {
            Ok(output) => match serde_json::to_value(output) {
                Ok(value) => CapabilityResponse::success(call_id.clone(), value),
                Err(error) => capability_error(
                    call_id,
                    CapabilityErrorCode::Internal,
                    &error.to_string(),
                    false,
                ),
            },
            Err(error) => capability_bro_error(call_id, error),
        });
    }
    let capability = state.runtime.session_agents_with_policy(
        routed.worker_id,
        session_id,
        SessionAttemptBinding::authenticated(
            routed.authorization.task_id,
            routed.authorization.attempt_id,
            routed.authorization.session_attempt_generation,
        ),
        invocation_id,
        routed.authorization.capability_policy,
    );
    let result: Result<Value, BroError> = match routed.request.operation.as_str() {
        "spawn" => {
            call_value::<AgentSpawnRequest, _, _, _>(
                routed.request.bounded_payload,
                |request| async { capability.spawn(request).await },
            )
            .await
        }
        "send_message" => {
            call_value::<AgentMessageRequest, _, _, _>(
                routed.request.bounded_payload,
                |request| async {
                    capability.send_message(request).await?;
                    Ok(json!({"accepted": true}))
                },
            )
            .await
        }
        "followup" => {
            call_value::<AgentMessageRequest, _, _, _>(
                routed.request.bounded_payload,
                |request| async {
                    capability.followup(request).await?;
                    Ok(json!({"accepted": true}))
                },
            )
            .await
        }
        "interrupt" => {
            call_value::<AgentTarget, _, _, _>(routed.request.bounded_payload, |request| async {
                capability.interrupt(request).await
            })
            .await
        }
        "status" => {
            call_value::<AgentTarget, _, _, _>(routed.request.bounded_payload, |request| async {
                capability.status(request).await
            })
            .await
        }
        "list" => {
            let prefix: Result<Option<String>, _> =
                serde_json::from_value(routed.request.bounded_payload);
            match prefix {
                Ok(prefix) => capability.list(prefix).await.and_then(to_value),
                Err(error) => Err(BroError::new("agent.invalid_request", error.to_string())),
            }
        }
        "wait" => {
            call_value::<AgentWaitRequest, _, _, _>(
                routed.request.bounded_payload,
                |request| async { capability.wait(request).await },
            )
            .await
        }
        operation => Err(BroError::new(
            "agent.invalid_operation",
            format!("unknown blackops.agent operation {operation}"),
        )),
    };
    Json(match result {
        Ok(value) => CapabilityResponse::success(call_id, value),
        Err(error) => capability_bro_error(call_id, error),
    })
}

async fn compat_dashboard(State(state): State<AppState>) -> BlackopsdResult<Json<Value>> {
    let snapshot = state.runtime.authority().snapshot().await?;
    let service = state.runtime.status().await?;
    Ok(Json(json!({
        "service": service,
        "agents": snapshot.agents.values().collect::<Vec<_>>(),
        "operations": snapshot.operations.values().collect::<Vec<_>>()
    })))
}

async fn compat_team(
    State(state): State<AppState>,
    Path(team): Path<String>,
) -> BlackopsdResult<Json<blackops_core::TeamRecord>> {
    let teams = state
        .runtime
        .authority()
        .call(|authority| Ok(authority.list_teams()))
        .await?;
    teams
        .into_iter()
        .find(|candidate| candidate.team_id.as_str() == team || candidate.name == team)
        .map(Json)
        .ok_or_else(|| BlackopsdError::InvalidRequest(format!("unknown team {team}")))
}

#[derive(Debug, Deserialize)]
struct BroadcastRequest {
    message: String,
    #[serde(default)]
    path_prefix: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
}

async fn compat_broadcast(
    State(state): State<AppState>,
    Json(request): Json<BroadcastRequest>,
) -> BlackopsdResult<Json<Value>> {
    if request.message.trim().is_empty() {
        return Err(BlackopsdError::InvalidRequest(
            "broadcast message must not be empty".into(),
        ));
    }
    let base = request
        .idempotency_key
        .unwrap_or_else(|| format!("broadcast-{}", uuid::Uuid::new_v4()));
    let delivered = state
        .runtime
        .authority()
        .call(move |authority| {
            let recipients: Vec<_> = authority
                .list_agents()
                .into_iter()
                .filter(|agent| agent.role != "session-root")
                .filter(|agent| {
                    request
                        .path_prefix
                        .as_ref()
                        .is_none_or(|prefix| agent.path.starts_with(prefix))
                })
                .collect();
            let mut delivered = 0usize;
            for agent in recipients {
                authority.send_message(SendMessageRequest {
                    idempotency_key: format!("{base}:{}", agent.agent_id),
                    sender: None,
                    recipient: agent.agent_id,
                    body: request.message.clone(),
                    created_at_unix_ms: now_ms(),
                })?;
                delivered += 1;
            }
            Ok(delivered)
        })
        .await?;
    Ok(Json(json!({"accepted": true, "delivered": delivered})))
}

fn session_capability<T>(
    runtime: &BlackopsRuntime,
    call: &AgentCall<T>,
) -> crate::SessionAgentCapability {
    runtime.session_agents(
        call.worker_id.clone(),
        SessionId::new(call.session_id.clone()),
        call.call_id.clone(),
    )
}

async fn call_value<T, U, F, Fut>(payload: Value, call: F) -> Result<Value, BroError>
where
    T: DeserializeOwned,
    U: Serialize,
    F: FnOnce(T) -> Fut,
    Fut: std::future::Future<Output = Result<U, BroError>>,
{
    let request = serde_json::from_value(payload)
        .map_err(|error| BroError::new("agent.invalid_request", error.to_string()))?;
    call(request).await.and_then(to_value)
}

fn to_value(value: impl Serialize) -> Result<Value, BroError> {
    serde_json::to_value(value).map_err(|error| BroError::new("agent.internal", error.to_string()))
}

fn capability_bro_error(call_id: String, error: BroError) -> CapabilityResponse {
    let (code, retryable) = if error.code.contains("deadline") {
        (CapabilityErrorCode::DeadlineExceeded, true)
    } else if error.code.contains("not_found") {
        (CapabilityErrorCode::InvalidRequest, false)
    } else if error.code.contains("conflict") {
        (CapabilityErrorCode::Conflict, false)
    } else if error.code.contains("invalid") {
        (CapabilityErrorCode::InvalidRequest, false)
    } else {
        (CapabilityErrorCode::Internal, false)
    };
    capability_error(call_id, code, &error.message, retryable)
}

fn capability_error(
    call_id: String,
    code: CapabilityErrorCode,
    message: &str,
    retryable: bool,
) -> CapabilityResponse {
    CapabilityResponse::error(
        call_id.clone(),
        CapabilityError {
            code,
            message: message.into(),
            retryable,
            details: None,
        },
    )
    .unwrap_or_else(|_| CapabilityResponse {
        call_id,
        result_or_error: json!({"message": message}),
        is_error: true,
    })
}

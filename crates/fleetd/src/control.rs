use std::collections::BTreeMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::Method;
use axum::middleware::{self, Next};
use axum::response::sse::{KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::{Json, Router, extract::DefaultBodyLimit};
use bro_capabilities::{
    AttemptOutcome, AttemptState, ExecutionAccepted, ExecutionCodeMode, ExecutionKind,
    ExecutionRequest, ExecutionServiceTier, ExecutionToolPolicy, WorkingSetIntent,
};
use bro_core::{AttemptId, OperationId, Provider, TaskId};
use bro_protocol::{
    AgentMailboxDelivery, AgentMailboxDeliveryReceipt, CloseoutOutcome, CloseoutRequest,
    RosterDelta, RosterSnapshotV1, ShutdownMode, TaskStatus, WorkerCommandKind,
};
use fleet_core::ProviderEnvironmentResolver;
use fleet_core::WorkerAuthorityState;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::authority_actor::AuthorityActor;
use crate::compatibility::{CompatibilityOwner, CompatibilityProxy};
use crate::launch::LaunchSpec;
use crate::roster::RosterHub;
use crate::roster::fleet_build_id;
use crate::shadow::{ParityReport, ShadowReplica};
use crate::worker::{LiveWorkers, deliver_task_command};
use crate::worktree::{WorktreeManager, looks_secret};
use crate::{FleetMode, FleetdError, FleetdResult, WorkerLauncher};

#[derive(Clone)]
pub(crate) struct AuthorityRuntime {
    pub authority: AuthorityActor,
    pub launcher: Arc<dyn WorkerLauncher>,
    pub live: LiveWorkers,
    pub provider_resolver: Arc<dyn ProviderEnvironmentResolver>,
    pub worktrees: WorktreeManager,
}

#[derive(Clone)]
pub(crate) struct ControlState {
    pub mode: FleetMode,
    pub roster: RosterHub,
    pub shadow: Option<ShadowReplica>,
    pub authority: Option<AuthorityRuntime>,
    pub compatibility: CompatibilityProxy,
    pub worker_socket_ready: Arc<std::sync::atomic::AtomicBool>,
    pub shadow_replication_enabled: bool,
    pub service_token: Arc<bro_rpc::ServiceToken>,
}

pub(crate) fn router(state: ControlState) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(health))
        .route("/readyz", get(ready))
        .route("/control/roster", get(roster_snapshot))
        .route("/control/roster/stream", get(roster_stream))
        .route("/control/roster/{task_id}", delete(forget_task))
        .route("/control/exec", post(exec_compat))
        .route("/control/resume", post(resume_compat))
        .route(
            "/control/closeout",
            post(closeout).layer(DefaultBodyLimit::max(crate::mcp::MAX_HTTP_BODY_BYTES)),
        )
        .route("/control/steer", post(steer))
        .route("/control/interrupt", post(interrupt))
        .route("/control/model", post(set_model))
        .route("/control/compact", post(compact))
        .route("/control/cancel", post(cancel))
        .route(
            "/control/broadcast",
            post(broadcast_compat).layer(DefaultBodyLimit::max(crate::mcp::MAX_HTTP_BODY_BYTES)),
        )
        .route("/control/dashboard", get(dashboard))
        .route("/control/team/{team_name}", get(team_compat))
        .route("/control/status/{task_id}", get(task_status))
        .route(
            "/mcp",
            post(crate::mcp::handle).layer(DefaultBodyLimit::max(crate::mcp::MAX_HTTP_BODY_BYTES)),
        )
        .route("/v1/executions", post(execute))
        .route("/v1/attempts/{attempt_id}", get(attempt_outcome))
        .route("/internal/agents/mailbox", post(deliver_agent_mailbox));
    if state.shadow_replication_enabled {
        router = router
            .route("/internal/shadow/roster", post(shadow_snapshot))
            .route("/internal/shadow/delta", post(shadow_delta))
            .route("/internal/shadow/compare", post(shadow_compare));
    }
    router
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_service_auth,
        ))
        .with_state(state)
}

async fn require_service_auth(
    State(state): State<ControlState>,
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

async fn closeout(
    State(state): State<ControlState>,
    Json(request): Json<CloseoutRequest>,
) -> FleetdResult<Json<CloseoutOutcome>> {
    Ok(Json(closeout_value(&state, request).await?))
}

pub(crate) async fn closeout_value(
    state: &ControlState,
    request: CloseoutRequest,
) -> FleetdResult<CloseoutOutcome> {
    let runtime = authority_runtime(state)?;
    let outcome = crate::closeout::run(&runtime.worktrees, request).await?;
    refresh_roster(&runtime.authority, &state.roster).await?;
    Ok(outcome)
}

async fn broadcast_compat(
    State(state): State<ControlState>,
    body: Bytes,
) -> FleetdResult<Response> {
    authority_runtime(&state)?;
    state
        .compatibility
        .forward(
            CompatibilityOwner::Blackopsd,
            Method::POST,
            &["control", "broadcast"],
            None,
            body,
        )
        .await
}

async fn team_compat(
    State(state): State<ControlState>,
    Path(team_name): Path<String>,
) -> FleetdResult<Response> {
    authority_runtime(&state)?;
    state
        .compatibility
        .forward(
            CompatibilityOwner::Blackopsd,
            Method::GET,
            &["control", "team", &team_name],
            None,
            Bytes::new(),
        )
        .await
}

async fn health(State(state): State<ControlState>) -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "fleetd",
        "mode": state.mode,
        "version": env!("CARGO_PKG_VERSION"),
        "build_id": fleet_build_id()
    }))
}

async fn ready(State(state): State<ControlState>) -> Json<Value> {
    let worker_socket = !state.mode.is_authority()
        || state
            .worker_socket_ready
            .load(std::sync::atomic::Ordering::Acquire);
    Json(json!({
        "ready": worker_socket,
        "mode": state.mode,
        "worker_socket": worker_socket,
        "build_id": fleet_build_id()
    }))
}

async fn roster_snapshot(State(state): State<ControlState>) -> Json<RosterSnapshotV1> {
    Json(state.roster.snapshot())
}

async fn roster_stream(
    State(state): State<ControlState>,
) -> Sse<impl futures::Stream<Item = Result<axum::response::sse::Event, std::convert::Infallible>>>
{
    Sse::new(state.roster.subscribe()).keep_alive(
        KeepAlive::new()
            .interval(std::time::Duration::from_secs(15))
            .text("fleetd"),
    )
}

async fn shadow_snapshot(
    State(state): State<ControlState>,
    Json(snapshot): Json<RosterSnapshotV1>,
) -> FleetdResult<Json<Value>> {
    let shadow = state.shadow.ok_or(FleetdError::ShadowReadOnly)?;
    shadow.apply_snapshot(snapshot);
    Ok(Json(json!({"ok": true})))
}

async fn shadow_delta(
    State(state): State<ControlState>,
    Json(delta): Json<RosterDelta>,
) -> FleetdResult<Json<Value>> {
    let shadow = state.shadow.ok_or_else(|| {
        FleetdError::Conflict("shadow replication endpoints are disabled in authority mode".into())
    })?;
    shadow.apply_delta(delta)?;
    Ok(Json(json!({"ok": true})))
}

async fn shadow_compare(
    State(state): State<ControlState>,
    Json(expected): Json<RosterSnapshotV1>,
) -> FleetdResult<Json<ParityReport>> {
    let shadow = state.shadow.ok_or_else(|| {
        FleetdError::Conflict("shadow comparison is disabled in authority mode".into())
    })?;
    Ok(Json(shadow.compare(&expected)))
}

async fn execute(
    State(state): State<ControlState>,
    Json(request): Json<ExecutionRequest>,
) -> FleetdResult<Json<ExecutionAccepted>> {
    let accepted = admit_and_launch(&state, request).await?;
    Ok(Json(accepted))
}

async fn attempt_outcome(
    State(state): State<ControlState>,
    Path(attempt_id): Path<String>,
) -> FleetdResult<Json<AttemptOutcome>> {
    let runtime = authority_runtime(&state)?;
    let attempt_id = AttemptId::new(attempt_id);
    let outcome = runtime
        .authority
        .call(move |authority| authority.attempt_outcome(&attempt_id).map_err(Into::into))
        .await?;
    Ok(Json(outcome))
}

async fn deliver_agent_mailbox(
    State(state): State<ControlState>,
    Json(delivery): Json<AgentMailboxDelivery>,
) -> FleetdResult<Json<AgentMailboxDeliveryReceipt>> {
    let runtime = authority_runtime(&state)?;
    let (worker_id, command, receipt) = runtime
        .authority
        .call(move |authority| {
            authority
                .enqueue_mailbox_delivery(delivery, now_ms())
                .map_err(Into::into)
        })
        .await?;
    if let Some(command) = command {
        // Persistence precedes the best-effort live send. A disconnected
        // worker receives the same command through handshake replay.
        let _ = runtime.live.send(&worker_id, command)?;
    }
    Ok(Json(receipt))
}

async fn exec_compat(
    State(state): State<ControlState>,
    Json(params): Json<LegacyExecParams>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(tool_result(exec_value(&state, params).await?)))
}

pub(crate) async fn exec_value(
    state: &ControlState,
    params: LegacyExecParams,
) -> FleetdResult<Value> {
    let request = params.into_request(None)?;
    let accepted = admit_and_launch(state, request).await?;
    Ok(json!({
        "taskId": accepted.task_id,
        "sessionId": accepted.session_id,
        "attemptId": accepted.attempt_id,
        "deduplicated": accepted.deduplicated
    }))
}

async fn resume_compat(
    State(state): State<ControlState>,
    Json(params): Json<LegacyExecParams>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(tool_result(resume_value(&state, params).await?)))
}

pub(crate) async fn resume_value(
    state: &ControlState,
    params: LegacyExecParams,
) -> FleetdResult<Value> {
    let session_id = params
        .session_id
        .clone()
        .ok_or_else(|| FleetdError::InvalidRequest("session_id is required for resume".into()))?;
    let request = params.into_request(Some(session_id))?;
    let accepted = admit_and_launch(state, request).await?;
    Ok(json!({
        "taskId": accepted.task_id,
        "sessionId": accepted.session_id,
        "attemptId": accepted.attempt_id,
        "deduplicated": accepted.deduplicated
    }))
}

async fn admit_and_launch(
    state: &ControlState,
    request: ExecutionRequest,
) -> FleetdResult<ExecutionAccepted> {
    let runtime = authority_runtime(state)?;
    if !request.provider.is_dispatchable() {
        return Err(FleetdError::InvalidRequest(
            "execution provider is not dispatchable".into(),
        ));
    }
    let initial_text = match &request.kind {
        ExecutionKind::Fresh { prompt } | ExecutionKind::Resume { prompt, .. } => {
            Some(prompt.clone())
        }
        ExecutionKind::MailboxResume { .. } => None,
    };
    if initial_text
        .as_deref()
        .is_some_and(|text| text.trim().is_empty())
    {
        return Err(FleetdError::InvalidRequest(
            "initial worker turn must not be empty".into(),
        ));
    }
    if let Some(key) = request.shell_env.keys().find(|key| looks_secret(key)) {
        return Err(FleetdError::InvalidRequest(format!(
            "shell_env key {key} is secret-shaped; provider credentials belong on the spawn-only provider lane"
        )));
    }
    let request_for_admission = request.clone();
    let accepted = runtime
        .authority
        .call(move |authority| {
            authority
                .admit_execution(request_for_admission, now_ms())
                .map_err(Into::into)
        })
        .await?;
    refresh_roster(&runtime.authority, &state.roster).await?;
    if accepted.deduplicated {
        let attempt_id = accepted.attempt_id.clone();
        let outcome = runtime
            .authority
            .call(move |authority| authority.attempt_outcome(&attempt_id).map_err(Into::into))
            .await?;
        if matches!(
            outcome.state,
            AttemptState::Completed
                | AttemptState::Failed
                | AttemptState::Interrupted
                | AttemptState::Lost
        ) {
            return Ok(accepted);
        }
    }

    let prepared = runtime.worktrees.prepare(&accepted, request).await;
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(error) => {
            fail_admitted_attempt(runtime, &accepted, &error).await?;
            return Err(error);
        }
    };
    let request = prepared.request;
    let preferred_account = request.labels.get("account").cloned();
    let task_id = accepted.task_id.clone();
    let model = request.model.clone();
    let allocation = runtime
        .authority
        .call(move |authority| {
            authority
                .reserve_provider(&task_id, preferred_account.as_deref(), &model, now_ms())
                .map_err(Into::into)
        })
        .await;
    let allocation = match allocation {
        Ok(allocation) => allocation,
        Err(error) => {
            if let Some(claim) = &prepared.claim {
                let _ = runtime.worktrees.cleanup(claim).await;
            }
            fail_admitted_attempt(runtime, &accepted, &error).await?;
            return Err(error);
        }
    };
    let provider_environment = match runtime.provider_resolver.resolve(&allocation) {
        Ok(environment) => environment,
        Err(error) => {
            let task_id = accepted.task_id.clone();
            let _ = runtime
                .authority
                .call(move |authority| {
                    authority
                        .release_provider(&task_id, now_ms())
                        .map(|_| ())
                        .map_err(Into::into)
                })
                .await;
            if let Some(claim) = &prepared.claim {
                let _ = runtime.worktrees.cleanup(claim).await;
            }
            let error = FleetdError::Authority(error);
            fail_admitted_attempt(runtime, &accepted, &error).await?;
            return Err(error);
        }
    };

    let task_id = accepted.task_id.clone();
    let existing = runtime
        .authority
        .call(move |authority| {
            let snapshot = authority.snapshot()?;
            Ok(snapshot
                .tasks
                .get(&task_id)
                .and_then(|task| task.worker_id.as_ref())
                .and_then(|worker_id| snapshot.workers.get(worker_id))
                .map(|worker| worker.state))
        })
        .await?;
    if existing.is_some_and(|state| {
        matches!(
            state,
            WorkerAuthorityState::AwaitingInitialConnection
                | WorkerAuthorityState::AwaitingReattach
                | WorkerAuthorityState::Active
                | WorkerAuthorityState::Draining
        )
    }) {
        return Ok(accepted);
    }
    // A duplicate accepted request can be replaying a crash after admission,
    // worktree claim, or provider reservation. Continue until worker authority
    // exists rather than returning an accepted-but-never-launched attempt.

    let task_id = accepted.task_id.clone();
    let provisioned = runtime
        .authority
        .call(move |authority| {
            match initial_text {
                Some(text) => authority
                    .provision_worker_with_initial_command(
                        &task_id,
                        WorkerCommandKind::UserTurn { text },
                        now_ms(),
                    )
                    .map(|provisioned| provisioned.worker),
                None => authority.provision_worker(&task_id, now_ms()),
            }
            .map_err(Into::into)
        })
        .await?;
    let launch = runtime
        .launcher
        .launch(LaunchSpec {
            accepted: accepted.clone(),
            request,
            provisioned,
            provider_environment,
        })
        .await;
    match launch {
        Ok(launched) => {
            let task_id = accepted.task_id.clone();
            let transcript_path = launched.event_log_path.to_string_lossy().into_owned();
            runtime
                .authority
                .call(move |authority| {
                    authority
                        .record_transcript_path(&task_id, transcript_path, now_ms())
                        .map_err(Into::into)
                })
                .await?;
        }
        Err(error) => {
            let attempt_id = accepted.attempt_id.clone();
            let detail = error.to_string();
            runtime
                .authority
                .call(move |authority| {
                    authority
                        .transition_attempt(
                            &attempt_id,
                            AttemptState::Failed,
                            json!({"error": detail}),
                            now_ms(),
                        )
                        .map_err(Into::into)
                })
                .await?;
            refresh_roster(&runtime.authority, &state.roster).await?;
            let task_id = accepted.task_id.clone();
            let _ = runtime
                .authority
                .call(move |authority| {
                    authority
                        .release_provider(&task_id, now_ms())
                        .map(|_| ())
                        .map_err(Into::into)
                })
                .await;
            if let Some(claim) = &prepared.claim {
                let _ = runtime.worktrees.cleanup(claim).await;
            }
            return Err(error);
        }
    }
    refresh_roster(&runtime.authority, &state.roster).await?;
    Ok(accepted)
}

async fn fail_admitted_attempt(
    runtime: &AuthorityRuntime,
    accepted: &ExecutionAccepted,
    error: &FleetdError,
) -> FleetdResult<()> {
    let attempt_id = accepted.attempt_id.clone();
    let detail = error.to_string();
    runtime
        .authority
        .call(move |authority| {
            authority
                .transition_attempt(
                    &attempt_id,
                    AttemptState::Failed,
                    json!({"error": detail}),
                    now_ms(),
                )
                .map_err(Into::into)
        })
        .await
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskPrompt {
    task_id: String,
    prompt: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskOnly {
    task_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelChange {
    task_id: String,
    model: String,
}

async fn steer(
    State(state): State<ControlState>,
    Json(input): Json<TaskPrompt>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(tool_result(steer_value(&state, input).await?)))
}

pub(crate) async fn steer_value(state: &ControlState, input: TaskPrompt) -> FleetdResult<Value> {
    let runtime = authority_runtime(state)?;
    let command = deliver_task_command(
        &runtime.authority,
        &runtime.live,
        &TaskId::new(input.task_id),
        WorkerCommandKind::Steer { text: input.prompt },
    )
    .await?;
    Ok(json!({"accepted": true, "command": command}))
}

async fn interrupt(
    State(state): State<ControlState>,
    Json(input): Json<TaskOnly>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(tool_result(interrupt_value(&state, input).await?)))
}

pub(crate) async fn interrupt_value(state: &ControlState, input: TaskOnly) -> FleetdResult<Value> {
    let runtime = authority_runtime(state)?;
    let command = deliver_task_command(
        &runtime.authority,
        &runtime.live,
        &TaskId::new(input.task_id),
        WorkerCommandKind::Interrupt,
    )
    .await?;
    Ok(json!({"accepted": true, "command": command}))
}

async fn set_model(
    State(state): State<ControlState>,
    Json(input): Json<ModelChange>,
) -> FleetdResult<Json<Value>> {
    let runtime = authority_runtime(&state)?;
    let command = deliver_task_command(
        &runtime.authority,
        &runtime.live,
        &TaskId::new(input.task_id),
        WorkerCommandKind::SetModel { model: input.model },
    )
    .await?;
    Ok(Json(tool_result(
        json!({"accepted": true, "command": command}),
    )))
}

async fn compact(
    State(state): State<ControlState>,
    Json(input): Json<TaskOnly>,
) -> FleetdResult<Json<Value>> {
    let runtime = authority_runtime(&state)?;
    let command = deliver_task_command(
        &runtime.authority,
        &runtime.live,
        &TaskId::new(input.task_id),
        WorkerCommandKind::Compact,
    )
    .await?;
    Ok(Json(tool_result(
        json!({"accepted": true, "command": command}),
    )))
}

async fn cancel(
    State(state): State<ControlState>,
    Json(input): Json<TaskOnly>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(tool_result(cancel_value(&state, input).await?)))
}

pub(crate) async fn cancel_value(state: &ControlState, input: TaskOnly) -> FleetdResult<Value> {
    let runtime = authority_runtime(state)?;
    let task_id = TaskId::new(input.task_id);
    let actor_task_id = task_id.clone();
    let (worker_id, command, status, deduplicated) = runtime
        .authority
        .call(move |authority| {
            let snapshot = authority.snapshot()?;
            let task = snapshot
                .tasks
                .get(&actor_task_id)
                .cloned()
                .ok_or_else(|| FleetdError::NotFound(format!("task {actor_task_id}")))?;
            if task.status.is_terminal() {
                return Ok((None, None, task.status, true));
            }
            if task.cancellation_requested_at_unix_ms.is_some() {
                return Ok((task.worker_id, None, task.status, true));
            }
            let command = authority.enqueue_command(
                &actor_task_id,
                WorkerCommandKind::Shutdown {
                    mode: ShutdownMode::Graceful,
                    deadline_unix_ms: Some(now_ms().saturating_add(30_000)),
                    reason: Some("operator cancellation".into()),
                },
                now_ms(),
            )?;
            let worker_id = authority
                .snapshot()?
                .tasks
                .get(&actor_task_id)
                .and_then(|task| task.worker_id.clone())
                .ok_or_else(|| FleetdError::NotFound(format!("worker for task {actor_task_id}")))?;
            Ok((Some(worker_id), Some(command), task.status, false))
        })
        .await?;
    if let (Some(worker_id), Some(command)) = (&worker_id, &command) {
        let _ = runtime.live.send(worker_id, command.clone());
    }
    Ok(json!({
        "accepted": true,
        "taskId": task_id,
        "status": status,
        "command": command,
        "deduplicated": deduplicated
    }))
}

async fn task_status(
    State(state): State<ControlState>,
    Path(task_id): Path<String>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(status_value(&state, &task_id)?))
}

pub(crate) fn status_value(state: &ControlState, task_id: &str) -> FleetdResult<Value> {
    let task_id = TaskId::new(task_id);
    let task = state
        .roster
        .snapshot()
        .tasks
        .into_iter()
        .find(|task| task.task_id == task_id)
        .ok_or_else(|| FleetdError::NotFound(format!("task {task_id}")))?;
    serde_json::to_value(task).map_err(Into::into)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WaitParams {
    pub(crate) task_id: String,
    #[serde(default)]
    pub(crate) timeout_seconds: Option<f64>,
}

pub(crate) async fn wait_value(state: &ControlState, input: WaitParams) -> FleetdResult<Value> {
    let seconds = input.timeout_seconds.unwrap_or(120.0);
    if !seconds.is_finite() || !(0.0..=900.0).contains(&seconds) {
        return Err(FleetdError::InvalidRequest(
            "timeout_seconds must be finite and between 0 and 900".into(),
        ));
    }
    let task_id = TaskId::new(input.task_id);
    let (completed, task) = state
        .roster
        .wait_for_terminal(&task_id, Duration::from_secs_f64(seconds))
        .await
        .ok_or_else(|| FleetdError::NotFound(format!("task {task_id}")))?;
    Ok(json!({
        "completed": completed,
        "timedOut": !completed,
        "task": task
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DashboardParams {
    #[serde(default)]
    pub(crate) provider: Option<String>,
    #[serde(default)]
    pub(crate) status: Option<String>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
    // Accepted for compatibility with the former operational dashboard query.
    #[serde(default, rename = "tail")]
    _tail: Option<usize>,
}

async fn dashboard(
    State(state): State<ControlState>,
    Query(params): Query<DashboardParams>,
) -> FleetdResult<Json<Value>> {
    Ok(Json(dashboard_value(&state, params)?))
}

pub(crate) fn dashboard_value(
    state: &ControlState,
    params: DashboardParams,
) -> FleetdResult<Value> {
    let provider = params
        .provider
        .as_deref()
        .map(Provider::from_str)
        .transpose()
        .map_err(|_| FleetdError::InvalidRequest("unknown provider filter".into()))?;
    let status = params
        .status
        .as_deref()
        .map(parse_task_status)
        .transpose()?;
    let limit = params.limit.unwrap_or(20).min(200);
    let mut tasks = state
        .roster
        .snapshot()
        .tasks
        .into_iter()
        .filter(|task| provider.is_none_or(|provider| task.provider == provider))
        .filter(|task| status.is_none_or(|status| task.status == status))
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| std::cmp::Reverse(task.started_at.unwrap_or_default()));
    tasks.truncate(limit);
    Ok(json!({"count": tasks.len(), "tasks": tasks}))
}

pub(crate) fn roster_value(state: &ControlState) -> FleetdResult<Value> {
    serde_json::to_value(state.roster.snapshot()).map_err(Into::into)
}

fn parse_task_status(value: &str) -> FleetdResult<TaskStatus> {
    match value {
        "pending" => Ok(TaskStatus::Pending),
        "running" => Ok(TaskStatus::Running),
        "completed" => Ok(TaskStatus::Completed),
        "failed" => Ok(TaskStatus::Failed),
        "cancelled" => Ok(TaskStatus::Cancelled),
        other => Err(FleetdError::InvalidRequest(format!(
            "unknown task status filter {other}"
        ))),
    }
}

async fn forget_task(
    State(state): State<ControlState>,
    Path(_task_id): Path<String>,
) -> FleetdResult<Json<Value>> {
    authority_runtime(&state)?;
    Err(FleetdError::Conflict(
        "roster forget requires the durable tombstone migration and is not available in this cutover window"
            .into(),
    ))
}

pub(crate) fn authority_runtime(state: &ControlState) -> FleetdResult<&AuthorityRuntime> {
    state.authority.as_ref().ok_or(FleetdError::ShadowReadOnly)
}

async fn refresh_roster(authority: &AuthorityActor, roster: &RosterHub) -> FleetdResult<()> {
    let snapshot = authority
        .call(|authority| authority.snapshot().map_err(Into::into))
        .await?;
    roster.publish_authority(&snapshot);
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct LegacyExecParams {
    provider: String,
    prompt: String,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    pin_model: Option<String>,
    #[serde(default)]
    pin_effort: Option<String>,
    #[serde(default)]
    code_mode: Option<String>,
    #[serde(default)]
    service_tier: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    idempotency_key: Option<String>,
    #[serde(default, rename = "allow_recursion")]
    _allow_recursion: Option<bool>,
    /// Trusted operator request for exact session-scoped remote authority.
    #[serde(default)]
    allowed_remote_operations: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    allowed_atom_refs: Vec<bro_core::AtomRef>,
}

impl LegacyExecParams {
    fn into_request(self, resume_session: Option<String>) -> FleetdResult<ExecutionRequest> {
        let provider = Provider::from_str(&self.provider)
            .map_err(|_| FleetdError::InvalidRequest("unknown provider".into()))?;
        let model = self
            .pin_model
            .or_else(|| provider.models().first().map(|model| model.id.to_string()))
            .ok_or_else(|| FleetdError::InvalidRequest("provider has no model".into()))?;
        let operation_id = OperationId::new(format!("operation-{}", uuid::Uuid::new_v4()));
        let kind = match resume_session {
            Some(session_id) => ExecutionKind::Resume {
                session_id: bro_core::SessionId::new(session_id),
                prompt: self.prompt,
            },
            None => ExecutionKind::Fresh {
                prompt: self.prompt,
            },
        };
        let service_tier = match self.service_tier.as_deref().unwrap_or("default") {
            "default" => ExecutionServiceTier::Default,
            "priority" => ExecutionServiceTier::Priority,
            "flex" => ExecutionServiceTier::Flex,
            value => {
                return Err(FleetdError::InvalidRequest(format!(
                    "unknown service tier {value}"
                )));
            }
        };
        let code_mode = self
            .code_mode
            .as_deref()
            .map(|value| match value {
                "off" => Ok(ExecutionCodeMode::Off),
                "optional" => Ok(ExecutionCodeMode::Optional),
                "only" => Ok(ExecutionCodeMode::Only),
                other => Err(FleetdError::InvalidRequest(format!(
                    "unknown code mode {other}"
                ))),
            })
            .transpose()?;
        let working_set = self
            .cwd
            .map(|cwd| WorkingSetIntent::Existing {
                cwd,
                managed_worktree: false,
            })
            .unwrap_or(WorkingSetIntent::Scratch);
        let mut labels = BTreeMap::from([("origin".into(), "cockpit".into())]);
        if let Some(name) = self.display_name {
            labels.insert("name".into(), name);
        }
        Ok(ExecutionRequest {
            operation_id,
            idempotency_key: self
                .idempotency_key
                .unwrap_or_else(|| format!("cockpit-{}", uuid::Uuid::new_v4())),
            kind,
            provider,
            model,
            effort: self.pin_effort,
            service_tier,
            code_mode,
            dispatch_context: None,
            working_set,
            shell_env: BTreeMap::new(),
            tool_policy: ExecutionToolPolicy {
                allowed_remote_operations: self.allowed_remote_operations,
                allowed_atom_refs: self.allowed_atom_refs,
                ..ExecutionToolPolicy::default()
            },
            system_prompt: None,
            output_schema: None,
            labels,
        })
    }
}

fn tool_result(value: Value) -> Value {
    json!({
        "content": [{"type": "text", "text": serde_json::to_string(&value).unwrap_or_default()}],
        "isError": false
    })
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_dispatch_admits_only_explicit_remote_authority() {
        let params: LegacyExecParams = serde_json::from_value(json!({
            "provider": "glm",
            "prompt": "inspect",
            "allowed_remote_operations": {
                "atom": ["invoke_atom"],
                "blackops.agent": ["spawn", "status"]
            },
            "allowed_atom_refs": ["atom:review@v1"]
        }))
        .unwrap();
        let request = params.into_request(None).unwrap();

        assert_eq!(
            request.tool_policy.allowed_remote_operations,
            BTreeMap::from([
                ("atom".into(), vec!["invoke_atom".into()]),
                (
                    "blackops.agent".into(),
                    vec!["spawn".into(), "status".into()]
                ),
            ])
        );
        assert_eq!(
            request.tool_policy.allowed_atom_refs,
            vec![bro_core::AtomRef::new("atom:review@v1")]
        );
    }

    #[test]
    fn legacy_dispatch_defaults_to_no_remote_authority() {
        let params: LegacyExecParams = serde_json::from_value(json!({
            "provider": "glm",
            "prompt": "inspect"
        }))
        .unwrap();
        let request = params.into_request(None).unwrap();

        assert!(request.tool_policy.allowed_remote_operations.is_empty());
        assert!(request.tool_policy.allowed_atom_refs.is_empty());
    }
}

//! Bro execution and Fleet control HTTP handlers.
//!
//! These routes delegate to the bro task/session plane and remain independent
//! of workflow admission, arc execution, and application event routing.

use std::path::PathBuf;
use std::sync::Arc;

use axum::extract::{Query, State as AxumState};
use axum::response::IntoResponse;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use serde::Deserialize;
use serde_json::json;

use super::state::{BlackboxServer, SharedState};
use crate::orchestration;
use crate::tools::bro_params::{
    BroadcastParams, CancelParams, DashboardParams, ExecParams, InterruptParams, ResumeParams,
    SteerParams,
};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ControlStatusQuery {
    #[serde(default)]
    tail: Option<usize>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

pub(crate) async fn control_exec_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(mut req): axum::Json<ExecParams>,
) -> axum::Json<CallToolResult> {
    // The HTTP control handler (`/control/exec`) bypasses the MCP
    // bro_exec tool surface but routes through the same spawn funnel.
    // Force the origin to Cockpit so the roster tab groups
    // cockpit-launched tasks separately from peer-bros-launched ones
    // (which carry AgentDispatch). The MCP bro_exec path itself
    // defaults to AgentDispatch and ignores this override slot.
    req.origin_override = Some(bro_core::Origin::Cockpit);
    axum::Json(BlackboxServer::new(state).bro_exec(Parameters(req)).await)
}

pub(crate) async fn control_resume_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(mut req): axum::Json<ResumeParams>,
) -> axum::Json<CallToolResult> {
    // Same origin pinning as `/control/exec`: a cockpit-driven resume is a
    // cockpit task. Without this the resumed follow-up turn spawns as
    // AgentDispatch and the roster row jumps from the fleet tab to the
    // dispatched-agents tab mid-conversation.
    req.origin_override = Some(bro_core::Origin::Cockpit);
    axum::Json(BlackboxServer::new(state).bro_resume(Parameters(req)).await)
}

// ── /control/closeout - phased closeout driver endpoint ────────────────────
//
// design/fleet-tui/closeout-command.md §4.1, Phase 3a (daemon-side). The
// endpoint applies the SAME pre-driver safety guards `exit_worktree` applies
// (managed-worktree, branch-prefix eligibility, detached-HEAD refusal,
// confirm gate) by reusing `bro_tools::fleet_worktree::prepare_closeout_request`
// (the shared entry extracted in Phase 1) - no silent duplication-with-drift.
// It then resolves `target` to the worktree's FORK-POINT branch (the branch it
// diverged from at dispatch, persisted in branch-scoped config) when the caller
// omits it, falling back to the base repo's current branch and then "main"
// (operator-decided default; the tool's default stays "main").
// Finally it calls `run_closeout_phases` and returns the STRUCTURED
// `CloseoutOutcome` directly (§4.3 - NOT a collapsed/rendered legacy tool
// JSON). Guard/validation failures return a 4xx with a clear error body.
pub(crate) async fn control_closeout_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<bro_protocol::CloseoutRequest>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use bro_tools::fleet_worktree::{
        CloseoutOutcome as ToolOutcome, prepare_closeout_request,
        run_closeout_phases_with_candidate_gate,
    };
    use serde_json::json;

    // Disposition whitelist (matches the existing tool's match arm).
    let disposition = req.disposition.trim().to_string();
    match disposition.as_str() {
        "keep" | "preflight" | "discard" | "publish" | "merge" | "adopt" => {}
        other => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({
                    "error": format!(
                        "disposition must be keep, preflight, discard, publish, merge, or adopt; got {other}"
                    ),
                })),
            )
                .into_response();
        }
    }
    // Mutating dispositions must carry confirm=true (same gate as the tool).
    if matches!(
        disposition.as_str(),
        "discard" | "publish" | "merge" | "adopt"
    ) && !req.confirm
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": format!("{disposition} requires confirm=true"),
            })),
        )
            .into_response();
    }
    // publish additionally needs a non-empty commit_message (preflight gate).
    if disposition == "publish"
        && req
            .commit_message
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
    {
        return (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({
                "error": "publish requires commit_message",
            })),
        )
            .into_response();
    }

    // Anchor base-repo resolution on the WORKTREE path, NOT the daemon CWD.
    //
    // In prod the daemon is launched from `WorkingDirectory=/Users/invidious`
    // (the launchd plist - NOT a git repo), and `BRO_FLEET_BASE_REPO` is not
    // in the plist env. If we passed the daemon CWD as `cx_root`,
    // `fleet_base_repo` would fall through to `primary_worktree`, which runs
    // `git -C <cx_root> worktree list` - that errors when `cx_root` is not
    // a git checkout, and every git-touching closeout (publish/merge/adopt/
    // discard/preflight) would fail in prod.
    //
    // The request's `worktree` is always a valid git worktree, so
    // `primary_worktree(<worktree>)` correctly returns the base/main
    // worktree regardless of where the daemon was launched. The tool path
    // behaves the same way: its `cx_root` is the worktree.
    let cx_root = PathBuf::from(&req.worktree);
    let worktree_arg: Option<&str> = if req.worktree.trim().is_empty() {
        None
    } else {
        Some(req.worktree.as_str())
    };

    // The cockpit creates managed worktrees under its fleet/agent store
    // (`bro_home/{fleet,agent}/worktrees`), NOT the legacy
    // `<repo_parent>/.bro-fleet-worktrees` convention the tool path assumes.
    // Pass those store roots so the managed-worktree guard accepts the
    // worktrees the cockpit actually produces (without them, `/closeout`
    // refuses every real fleet worktree - dogfooding finding).
    let extra_managed_roots = crate::managed_worktrees::cockpit_managed_worktree_roots();

    // Shared pre-driver guard: managed-worktree, branch-prefix, detached-HEAD,
    // target resolution. The endpoint's target default is the worktree's
    // fork-point branch, then the base repo's current branch, then "main"
    // (operator-decided) - the tool's default stays "main".
    let mut driver_req = match prepare_closeout_request(
        &cx_root,
        worktree_arg,
        |base_repo| match req.target.as_deref() {
            Some(t) if !t.trim().is_empty() => t.trim().to_string(),
            // Default to the branch this worktree diverged from at dispatch
            // (fork-point, persisted in branch-scoped config), which is immune
            // to base-repo HEAD movement. Fall back to the base repo's current
            // branch only when no fork-point was captured (legacy worktrees /
            // tool-path dispatch), then to "main".
            _ => bro_tools::fleet_worktree::fleet_base_branch(&cx_root)
                .or_else(|| bro_tools::fleet_worktree::current_branch(base_repo).ok())
                .unwrap_or_else(|| "main".to_string()),
        },
        req.allow_branch_prefixes.clone(),
        &extra_managed_roots,
    ) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error": format!("{e:#}")})),
            )
                .into_response();
        }
    };

    // Stamp the caller-supplied intent (guard doesn't see disposition/confirm/etc.)
    driver_req.disposition = disposition.clone();
    driver_req.confirm = req.confirm;
    driver_req.commit_message = req.commit_message.clone();
    driver_req.paths = req.paths.clone();
    driver_req.dry_run = req.dry_run;
    let was_dry_run = driver_req.dry_run;
    // Resolved project closeout hooks (the cockpit strict-loaded fleet.json and
    // sent them fully resolved). Translate the wire shape into the bro_tools
    // local type; the driver fires them at phase boundaries. Skipped on dry_run.
    driver_req.closeout_hooks = req.closeout_hooks.as_ref().map(to_driver_hooks);
    let teardown_checkout_id = bbox_corpus_core::identity::read_checkout_id(
        &driver_req.worktree.join(".bbox/local/checkout-id"),
    )
    .ok()
    .flatten();

    // The closeout phases shell out to sync git (fetch/rebase/merge/push/
    // worktree-remove - seconds to minutes) plus closeout hook scriptlets;
    // run on the blocking pool, never inline on a runtime worker (I2,
    // concurrency-model §5 Phase 4).
    let outcome = tokio::task::spawn_blocking(move || {
        let gate = |candidate: &bro_tools::fleet_worktree::CandidateTree| {
            super::knowledge_merge_gate::evaluate(candidate)
        };
        run_closeout_phases_with_candidate_gate(&driver_req, Some(&gate))
    })
    .await
    .unwrap_or_else(|join_err| {
        ToolOutcome::Failed(bro_tools::fleet_worktree::PhaseResult {
            phase: bro_tools::fleet_worktree::CloseoutPhase::Preflight,
            repo_cwd: std::path::PathBuf::new(),
            ok: false,
            error_class: bro_tools::fleet_worktree::CloseoutErrorClass::None,
            content: serde_json::json!({
                "error": format!("closeout driver task failed: {join_err}"),
            }),
        })
    });

    let checkout_removed = matches!(
        &outcome,
        ToolOutcome::Success { phases }
            if phases.iter().any(|phase| {
                phase.ok
                    && phase.phase == bro_tools::fleet_worktree::CloseoutPhase::Remove
            })
    );
    if let Some(checkout_id) = teardown_checkout_id {
        let server = BlackboxServer::new(state);
        if checkout_removed {
            let cleanup_checkout_id = checkout_id.clone();
            match tokio::task::spawn_blocking(move || {
                server.deregister_dark_knowledge_checkout(&cleanup_checkout_id)
            })
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    tracing::warn!(
                        checkout_id,
                        error = %err,
                        "closeout removed checkout but registry teardown failed; periodic reconciliation will retry"
                    );
                }
                Err(err) => {
                    tracing::warn!(
                        checkout_id,
                        error = %err,
                        "closeout removed checkout but registry teardown task failed; periodic reconciliation will retry"
                    );
                }
            }
        } else if !was_dry_run {
            // The local target can advance even when a later push or removal
            // fails. Refresh after every mutating closeout attempt so promotion
            // never waits for the committed-tree cache TTL in that case.
            if let Err(err) = tokio::task::spawn_blocking(move || {
                server.refresh_published_knowledge_for_checkout(&checkout_id)
            })
            .await
            {
                tracing::warn!(
                    error = %err,
                    "closeout checkout refresh task failed; periodic reconciliation will retry"
                );
            }
        }
    }

    // Translate bro_tools::CloseoutOutcome into the bro_protocol wire shape.
    // The two type families are intentionally distinct (bro_protocol is
    // contract-bottom; bro_tools stays free of it). Each field maps 1:1;
    // the daemon does the conversion. PathBuf serializes to a string under
    // serde, and serde_json::Value passes through as embedded JSON.
    let wire_outcome: bro_protocol::CloseoutOutcome = match &outcome {
        ToolOutcome::Success { phases } => bro_protocol::CloseoutOutcome::Success {
            phases: phases.iter().map(to_wire_phase).collect(),
        },
        ToolOutcome::Failed(r) => bro_protocol::CloseoutOutcome::Failed(to_wire_phase(r)),
    };

    // The status code reflects whether the endpoint reached the driver, not
    // the success of the driver (which already failed above with 4xx). A
    // failed phase is still a valid HTTP outcome - the structured error
    // class is the signal the cockpit routes on, not the HTTP status.
    let status = StatusCode::OK;
    (status, axum::Json(json!(wire_outcome))).into_response()
}

/// Map `bro_tools::fleet_worktree::CloseoutPhase` →
/// `bro_protocol::CloseoutPhase` by name (both are `Copy` + `Serialize` +
/// `Deserialize` and use the same `snake_case` rename, so the numeric
/// discriminants line up - but the type families are independent and the
/// daemon bridges them, per the design).
fn to_wire_phase(r: &bro_tools::fleet_worktree::PhaseResult) -> bro_protocol::PhaseResult {
    use bro_protocol::CloseoutErrorClass as WireErr;
    use bro_protocol::CloseoutPhase as WirePhase;
    use bro_tools::fleet_worktree::{CloseoutErrorClass as ToolErr, CloseoutPhase as ToolPhase};
    let phase = match r.phase {
        ToolPhase::Preflight => WirePhase::Preflight,
        ToolPhase::StageCommit => WirePhase::StageCommit,
        ToolPhase::FfBase => WirePhase::FfBase,
        ToolPhase::Rebase => WirePhase::Rebase,
        ToolPhase::MergeGate => WirePhase::MergeGate,
        ToolPhase::FfMerge => WirePhase::FfMerge,
        ToolPhase::Push => WirePhase::Push,
        ToolPhase::Remove => WirePhase::Remove,
        ToolPhase::Hook => WirePhase::Hook,
    };
    let error_class = match r.error_class {
        ToolErr::None => WireErr::None,
        ToolErr::BaseNotReady => WireErr::BaseNotReady,
        ToolErr::FfBaseFailed => WireErr::FfBaseFailed,
        ToolErr::StageFailed => WireErr::StageFailed,
        ToolErr::CommitFailed => WireErr::CommitFailed,
        ToolErr::RebaseConflict => WireErr::RebaseConflict,
        ToolErr::MergeGateBlocked => WireErr::MergeGateBlocked,
        ToolErr::FfMergeFailed => WireErr::FfMergeFailed,
        ToolErr::PushRejected => WireErr::PushRejected,
        ToolErr::RemoveFailed => WireErr::RemoveFailed,
        ToolErr::HookBlocked => WireErr::HookBlocked,
        ToolErr::Other => WireErr::Other,
    };
    bro_protocol::PhaseResult {
        phase,
        repo_cwd: r.repo_cwd.clone(),
        ok: r.ok,
        error_class,
        content: r.content.clone(),
    }
}

/// Translate the resolved wire `CloseoutHooksWire` into the `bro_tools` local
/// `CloseoutHooks` the phased driver consumes. The daemon bridges the two type
/// families (bro_protocol is contract-bottom; bro_tools stays free of it).
fn to_driver_hooks(
    w: &bro_protocol::CloseoutHooksWire,
) -> bro_tools::fleet_worktree::CloseoutHooks {
    use bro_tools::fleet_worktree::{CloseoutHooks, HookOnFail};
    let on_fail = match w.on_fail.as_deref() {
        Some("block") => HookOnFail::Block,
        _ => HookOnFail::Warn,
    };
    CloseoutHooks {
        hooks: w.hooks.clone(),
        cwd: w.cwd.as_ref().map(std::path::PathBuf::from),
        on_fail,
        timeout_secs: w.timeout_secs.unwrap_or(600),
    }
}

pub(crate) async fn control_steer_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<SteerParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_steer(Parameters(req)))
}

pub(crate) async fn control_interrupt_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<InterruptParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_interrupt(Parameters(req)))
}

pub(crate) async fn control_broadcast_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<BroadcastParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(
        BlackboxServer::new(state)
            .bro_broadcast(Parameters(req))
            .await,
    )
}

pub(crate) async fn control_status_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
    Query(query): Query<ControlStatusQuery>,
) -> axum::Json<CallToolResult> {
    let task = state.task_store.read().get(&task_id);
    axum::Json(match task {
        Some(task) => match orchestration::control_task_status_json(
            &task,
            query.detail.as_deref().unwrap_or("summary"),
            query.cursor.as_deref(),
            query.limit,
            query.tail.unwrap_or(0),
        ) {
            Ok(value) => BlackboxServer::ok_json(&value),
            Err(error) => BlackboxServer::err_text(&error.to_string()),
        },
        None => BlackboxServer::err_text(&format!("Unknown task ID: {task_id}")),
    })
}

// ── /control/roster - daemon-authoritative fleet roster snapshot ─────────
//
// Slice 1a of design/fleet-tui/daemon-roster-and-tail-unification.md §3
// item 2. A pure-addition read-only endpoint that projects every task
// currently in `state.task_store` into a `RosterSummaryV1` and wraps
// them in a versioned `RosterSnapshotV1` envelope. No new task fields,
// no client-side change yet, no SSE - that is Slice 2.
//
// Two design notes for the derivation:
//
// 1. `model` is best-effort, scanned from the task's recorded event
//    buffer. The fleet client uses the same logic at
//    `crates/bro-fleet-client/src/fleet.rs:1355-1363`; we inline the
//    scan here because the touched-file list for this slice is
//    daemon-side only (the design explicitly defers client changes).
//
// 2. `last_event_at` is NOT a stored field - the daemon has no
//    per-event arrival stamp on V1 (the client stamps it from
//    `eventCount` growth today, fleet.rs:1301). We derive it from
//    `max(started_at, completed_at)`: for a live task this is the
//    spawn time, for a terminal task this is the completion time.
//    Coarse, but it stays within "no new task fields" and the
//    derivation is documented in the DTO doc-comment in
//    `crates/bro-protocol/src/lib.rs`.
pub(crate) async fn control_roster_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
) -> impl axum::response::IntoResponse {
    use axum::response::IntoResponse;
    use bro_protocol::RosterSnapshotV1;

    // Read the view first, then the generation. `RosterEventSink`
    // builds the summary, writes the view, *then* bumps the
    // generation, so reading the version after the snapshot means
    // a snapshot's `version` is never older than the tasks it lists.
    // The lock here is the view's brief read guard, NOT a per-task
    // inner mutex - fleet polling no longer contends with event
    // ingest on busy tasks (wave 6a).
    let tasks = state.roster_view.snapshot();
    let version = state.roster_events().current_version();

    // D27: stamp the daemon's build identity on the snapshot so the
    // fleet cockpit can detect long-lived cockpits still running
    // stale binaries across upgrades. Both fields are additive
    // `Option<String>` - older daemons that pre-date the build.rs
    // and the protocol field simply emit `None` and the cockpit
    // treats the snapshot as identity-unknown (zero visual change).
    let daemon_version = Some(env!("CARGO_PKG_VERSION").to_string());
    let daemon_build_id = Some(env!("BLACKBOX_BUILD_ID").to_string());

    let snapshot = RosterSnapshotV1 {
        version,
        tasks,
        daemon_version,
        daemon_build_id,
    };
    axum::Json(snapshot).into_response()
}

pub(crate) async fn control_roster_forget_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(task_id): axum::extract::Path<String>,
) -> axum::response::Response {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use serde_json::json;

    let dropped = {
        let mut store = state.task_store.write();
        let Some(task) = store.get(&task_id) else {
            return (
                StatusCode::NOT_FOUND,
                axum::Json(json!({
                    "error": format!("task {task_id} is not in the roster"),
                })),
            )
                .into_response();
        };
        let status = task.inner.lock().status;
        if !status.is_terminal() {
            return (
                StatusCode::CONFLICT,
                axum::Json(json!({
                    "error": format!(
                        "task {task_id} is {status:?}; only terminal tasks can be forgotten"
                    ),
                })),
            )
                .into_response();
        }
        store.retain_drop(|task| task.id() != task_id)
    };

    if dropped.is_empty() {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(json!({
                "error": format!("task {task_id} is not in the roster"),
            })),
        )
            .into_response();
    }

    orchestration::request_persist(&state.task_store, &state.store_dir);
    state.roster_events().emit_removed(task_id.clone());

    axum::Json(json!({
        "taskId": task_id,
        "forgotten": true,
    }))
    .into_response()
}

pub(crate) async fn control_dashboard_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    Query(query): Query<DashboardParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_dashboard(Parameters(query)))
}

pub(crate) async fn control_cancel_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::Json(req): axum::Json<CancelParams>,
) -> axum::Json<CallToolResult> {
    axum::Json(BlackboxServer::new(state).bro_cancel(Parameters(req)))
}

pub(crate) async fn control_team_handler(
    AxumState(state): AxumState<Arc<SharedState>>,
    axum::extract::Path(team_name): axum::extract::Path<String>,
) -> impl axum::response::IntoResponse {
    match orchestration::team::load_team(&team_name, &state.store_dir) {
        Some(team) => axum::Json(json!({
            "team": team.name,
            "members": team.members.iter().map(|m| m.name.clone()).collect::<Vec<_>>(),
        }))
        .into_response(),
        None => (
            axum::http::StatusCode::NOT_FOUND,
            format!("unknown team: {team_name}"),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::orchestration::providers::Provider;

    /// /control/closeout (Phase 3a, design/fleet-tui/closeout-command.md §4.1)
    /// validates the request before reaching the driver: a disposition not in
    /// the keep/preflight/discard/publish/merge/adopt set returns 400 with a
    /// clear error body. This is the cheapest guard-level assertion - the
    /// other guards (unmanaged worktree, detached HEAD, branch prefix)
    /// require git setup and are covered by the bro-tools unit tests.
    #[tokio::test]
    async fn control_closeout_rejects_unknown_disposition() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "definitely-not-a-real-disposition".to_string(),
            confirm: true,
            target: None,
            commit_message: None,
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "unknown disposition must yield 400"
        );
    }

    /// /control/closeout enforces the confirm gate on mutating dispositions
    /// (discard/publish/merge/adopt), matching `exit_worktree` exactly.
    /// publish without confirm returns 400.
    #[tokio::test]
    async fn control_closeout_publish_without_confirm_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "publish".to_string(),
            confirm: false,
            target: None,
            commit_message: Some("test commit".to_string()),
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "publish without confirm must yield 400"
        );
    }

    /// /control/closeout also enforces publish's commit_message gate, matching
    /// the tool's preflight bail ("publish requires commit_message").
    #[tokio::test]
    async fn control_closeout_publish_without_commit_message_returns_400() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let req = bro_protocol::CloseoutRequest {
            worktree: tmp.path().to_string_lossy().into_owned(),
            disposition: "publish".to_string(),
            confirm: true,
            target: None,
            commit_message: None,
            paths: vec![],
            allow_branch_prefixes: None,
            dry_run: false,
            closeout_hooks: None,
        };
        let resp = control_closeout_handler(AxumState(state), axum::Json(req))
            .await
            .into_response();
        assert_eq!(
            resp.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "publish without commit_message must yield 400"
        );
    }

    // ── /control/roster - Slice 1a focused test ──────────────────────────
    //
    // Spec: the DTO serializes with the expected fields and has NO
    // events field (regression guard for the cf87a52 truncation
    // class); the handler returns one summary per task in
    // state.task_store. The DTO field-shape assertion lives in
    // `crates/bro-protocol/src/lib.rs::tests::roster_summary_v1_serializes_without_events_field`;
    // this test focuses on the handler: insert N tasks, get back N
    // summaries, none of them carrying an events array.
    #[tokio::test]
    async fn control_roster_returns_one_summary_per_task() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Seed two tasks - one running, one completed - so we can also
        // see that the status mapping is consistent (not asserted
        // here; the DTO test owns the field shape, this test owns
        // the count + absence of events).
        {
            let mut store = state.task_store.write();
            store
                .insert(
                    "task-a".to_string(),
                    orchestration::test_task(
                        "task-a",
                        orchestration::TaskStatus::Running,
                        Provider::Glm,
                    ),
                )
                .expect("insert task-a");
            store
                .insert(
                    "task-b".to_string(),
                    orchestration::test_task(
                        "task-b",
                        orchestration::TaskStatus::Completed,
                        Provider::Deepseek,
                    ),
                )
                .expect("insert task-b");
        }
        // Wave 6a: the handler serves from the RosterView. Tests
        // that bypass the spawn path (and its emit_added) must seed
        // the view the same way cold-start does.
        state
            .roster_view
            .rebuild_from_store(&state.task_store.read());

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value =
            serde_json::from_slice(&body_bytes).expect("roster body must be valid JSON");
        let tasks = value
            .get("tasks")
            .and_then(|t| t.as_array())
            .expect("envelope must carry a `tasks` array");
        assert_eq!(
            tasks.len(),
            2,
            "expected one RosterSummaryV1 per task in the store, got {tasks:?}"
        );

        // Per-summary regression guard: no events field, no
        // recentEvents field, no array-typed field at all (a Vec
        // empty-or-otherwise would reopen the 80KB truncation class).
        for (i, summary) in tasks.iter().enumerate() {
            let obj = summary
                .as_object()
                .unwrap_or_else(|| panic!("summary[{i}] must be an object"));
            assert!(
                !obj.contains_key("events"),
                "summary[{i}] must NOT carry an `events` field (cf87a52 regression guard)"
            );
            assert!(
                !obj.contains_key("recentEvents"),
                "summary[{i}] must NOT carry a `recentEvents` field"
            );
            for (k, v) in obj {
                assert!(
                    !v.is_array(),
                    "summary[{i}].{k} unexpectedly serialized as array: {v}"
                );
            }
        }

        // Envelope shape: { version, tasks, daemon_version?,
        // daemon_build_id? }. Version is the roster generation; this
        // test asserts only presence, while the Slice 2 delta test
        // owns generation semantics. The two `daemon_*` build-identity
        // fields were added in D27 (unit-N4 thread-c3f7c7e3) as
        // `#[serde(default, skip_serializing_if = "Option::is_none")]`
        // additivities - when populated, they ride along on the
        // envelope; when `None`, the wire shape is unchanged.
        let obj = value.as_object().expect("envelope must be an object");
        assert!(obj.contains_key("version"), "envelope must carry `version`");
        assert!(obj.contains_key("tasks"), "envelope must carry `tasks`");
        // The two additive identity fields, when present, must be
        // stringly-typed (a regression of the additive DTO).
        for key in ["daemon_version", "daemon_build_id"] {
            if let Some(v) = obj.get(key) {
                assert!(
                    v.is_string(),
                    "envelope.{key} must be a string when present, got: {v}"
                );
            }
        }
    }

    #[tokio::test]
    async fn control_roster_forget_drops_only_terminal_tasks_from_snapshot() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        {
            let mut store = state.task_store.write();
            store
                .insert(
                    "running".to_string(),
                    orchestration::test_task(
                        "running",
                        orchestration::TaskStatus::Running,
                        Provider::Glm,
                    ),
                )
                .expect("insert running");
            store
                .insert(
                    "terminal".to_string(),
                    orchestration::test_task(
                        "terminal",
                        orchestration::TaskStatus::Completed,
                        Provider::Brodex,
                    ),
                )
                .expect("insert terminal");
        }
        // Wave 6a: see note in `control_roster_returns_one_summary_per_task`.
        state
            .roster_view
            .rebuild_from_store(&state.task_store.read());

        let running_resp = control_roster_forget_handler(
            AxumState(state.clone()),
            axum::extract::Path("running".to_string()),
        )
        .await;
        assert_eq!(running_resp.status(), axum::http::StatusCode::CONFLICT);

        let terminal_resp = control_roster_forget_handler(
            AxumState(state.clone()),
            axum::extract::Path("terminal".to_string()),
        )
        .await;
        assert_eq!(terminal_resp.status(), axum::http::StatusCode::OK);

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: bro_protocol::RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        let ids: Vec<String> = snapshot
            .tasks
            .into_iter()
            .map(|task| task.task_id.as_str().to_string())
            .collect();

        assert_eq!(ids, vec!["running".to_string()]);
    }

    /// D27: the daemon's `/control/roster` snapshot now stamps
    /// `daemon_version` and `daemon_build_id` so a long-lived
    /// cockpit can detect when the daemon was rebuilt underneath
    /// it. Both values are sourced from compile-time env!() at the
    /// daemon's link step (root `build.rs`).
    #[tokio::test]
    async fn control_roster_handler_stamps_daemon_build_identity() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: bro_protocol::RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");

        // Both fields are present and equal the daemon's compile-time
        // identity. A pure env!() comparison - no filesystem probing.
        assert_eq!(
            snapshot.daemon_version.as_deref(),
            Some(env!("CARGO_PKG_VERSION")),
            "daemon_version must reflect CARGO_PKG_VERSION at the daemon's link time"
        );
        assert_eq!(
            snapshot.daemon_build_id.as_deref(),
            Some(env!("BLACKBOX_BUILD_ID")),
            "daemon_build_id must reflect BLACKBOX_BUILD_ID from the root build.rs"
        );

        // The two values must parse as a non-empty string each
        // (we don't pin specific values - only the contract that
        // the snapshot is identity-stamped, not identity-unknown).
        assert!(
            snapshot
                .daemon_build_id
                .as_deref()
                .is_some_and(|s| !s.is_empty()),
            "BLACKBOX_BUILD_ID must be a non-empty stamp"
        );
    }

    #[tokio::test]
    async fn control_roster_generation_is_shared_by_snapshot_and_deltas() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::{RosterDelta, RosterSnapshotV1};
        use tokio::time::{Duration, timeout};

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let mut rx = state.roster_tx.subscribe();

        let task = orchestration::spawn_in_process_task(
            "task-roster-generation".to_string(),
            Provider::Workflow,
            "session-roster-generation".to_string(),
            None,
            state.store_dir.clone(),
            state.task_store.clone(),
            state.tail_tx.clone(),
            Some(state.roster_events()),
            Some("roster-test".to_string()),
            None,
            Some(state.system_events.clone()),
            bro_core::Origin::Workflow,
        );

        let added = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("added delta should arrive")
            .expect("roster channel open");
        let added_seq = added.seq();
        match added {
            RosterDelta::Added { seq, task } => {
                assert_eq!(seq, 1, "first roster mutation should use generation 1");
                assert_eq!(task.task_id.as_str(), "task-roster-generation");
            }
            other => panic!("expected Added delta, got {other:?}"),
        }

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        assert_eq!(
            snapshot.version, added_seq,
            "snapshot version must read the same generation counter as deltas"
        );

        orchestration::push_in_process_event(
            &task,
            serde_json::json!({"type":"assistant","message":{"model":"test-model"}}),
            &state.tail_tx,
        );
        let updated = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("updated delta should arrive")
            .expect("roster channel open");
        match updated {
            RosterDelta::Updated { seq, task } => {
                assert_eq!(seq, snapshot.version + 1);
                assert_eq!(task.task_id.as_str(), "task-roster-generation");
                assert_eq!(task.model.as_deref(), Some("test-model"));
            }
            other => panic!("expected Updated delta, got {other:?}"),
        }

        orchestration::finish_in_process_task(
            &task,
            orchestration::TaskStatus::Completed,
            Some("done".to_string()),
            None,
            &state.task_store,
            &state.store_dir,
            &state.tail_tx,
            Some(state.system_events.clone()),
        );
        let terminal_update = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("terminal update delta should arrive")
            .expect("roster channel open");
        assert!(
            matches!(terminal_update, RosterDelta::Updated { .. }),
            "terminal status transition should emit Updated, got {terminal_update:?}"
        );

        let server = BlackboxServer::new(state.clone());
        server
            .bro_prune(Parameters(crate::tools::bro_params::PruneParams {
                status: None,
                provider: None,
                older_than_hours: None,
                dry_run: None,
                task_ids: Some(vec!["task-roster-generation".to_string()]),
                retro: None,
                retro_min_turns: None,
                retro_max: None,
            }))
            .await;
        let removed = timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("removed delta should arrive")
            .expect("roster channel open");
        match removed {
            RosterDelta::Removed { seq, task_id } => {
                assert!(seq > terminal_update.seq());
                assert_eq!(task_id.as_str(), "task-roster-generation");
            }
            other => panic!("expected Removed delta, got {other:?}"),
        }
    }

    // Slice 1b - `/control/roster` must surface the spawn-time
    // `origin` for every task so the roster UI can tab Fleet vs
    // Dispatched vs Workflow vs Atom without re-deriving. The
    // handler reads `inner.origin` (Slice 1b plumbing) and projects
    // it onto `RosterSummaryV1.origin` (added in Slice 1b); this
    // test seeds two tasks with explicit different origins and
    // asserts the per-task summary carries the right one.
    #[tokio::test]
    async fn control_roster_projects_origin_per_task() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Seed two tasks with deliberately different origins so a
        // mis-projection (e.g. always defaulting to `unknown` or
        // always reading the same task) would be visible.
        {
            let mut store = state.task_store.write();
            let task_a = orchestration::test_task(
                "task-cockpit",
                orchestration::TaskStatus::Running,
                Provider::Glm,
            );
            task_a.inner.lock().origin = bro_core::Origin::Cockpit;
            store
                .insert("task-cockpit".to_string(), task_a)
                .expect("insert task-cockpit");

            let task_b = orchestration::test_task(
                "task-workflow",
                orchestration::TaskStatus::Running,
                Provider::Glm,
            );
            {
                let mut inner = task_b.inner.lock();
                inner.origin = bro_core::Origin::Workflow;
                inner.workflow_owned = orchestration::workflow_owned_for_origin(inner.origin);
            }
            store
                .insert("task-workflow".to_string(), task_b)
                .expect("insert task-workflow");
        }
        // Wave 6a: see note in `control_roster_returns_one_summary_per_task`.
        state
            .roster_view
            .rebuild_from_store(&state.task_store.read());

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let value: serde_json::Value = serde_json::from_slice(&body_bytes).unwrap();
        let tasks = value
            .get("tasks")
            .and_then(|t| t.as_array())
            .expect("envelope must carry a `tasks` array");
        assert_eq!(tasks.len(), 2);

        // Per-summary `origin` projection check. We index by
        // task_id (stable) rather than array order (HashMap-iteration
        // order).
        let mut by_id = std::collections::HashMap::new();
        for s in tasks {
            let obj = s.as_object().expect("summary must be object");
            let task_id = obj
                .get("task_id")
                .and_then(|v| v.as_str())
                .expect("summary must carry task_id")
                .to_string();
            let origin = obj
                .get("origin")
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| panic!("summary[{task_id}] must carry lowercase origin"))
                .to_string();
            by_id.insert(task_id, origin);
        }

        assert_eq!(
            by_id.get("task-cockpit").map(String::as_str),
            Some("cockpit"),
            "cockpit-origin task must project as \"cockpit\" on the roster"
        );
        assert_eq!(
            by_id.get("task-workflow").map(String::as_str),
            Some("workflow"),
            "workflow-origin task must project as \"workflow\" on the roster"
        );
    }

    #[tokio::test]
    async fn control_roster_surfaces_worktree_and_workflow_owned_metadata() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::RosterSnapshotV1;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));
        let task = orchestration::test_task(
            "task-meta",
            orchestration::TaskStatus::Running,
            Provider::Workflow,
        );
        {
            let mut inner = task.inner.lock();
            inner.managed_worktree = Some("/tmp/managed/task-meta".to_string());
            inner.origin = bro_core::Origin::Workflow;
            inner.workflow_owned = orchestration::workflow_owned_for_origin(inner.origin);
        }
        state
            .task_store
            .write()
            .insert("task-meta".to_string(), task)
            .expect("insert task-meta");
        // Wave 6a: see note in `control_roster_returns_one_summary_per_task`.
        state
            .roster_view
            .rebuild_from_store(&state.task_store.read());

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");
        let summary = snapshot
            .tasks
            .iter()
            .find(|task| task.task_id.as_str() == "task-meta")
            .expect("task-meta should be present");
        assert_eq!(
            summary.managed_worktree.as_deref(),
            Some("/tmp/managed/task-meta")
        );
        assert!(summary.workflow_owned);
    }

    // ── /control/roster - wave 6a RosterView contract ────────────────────
    //
    // The endpoint serves from `SharedState::roster_view`, not from
    // iterating `task_store` and re-locking each inner mutex. These
    // tests pin the wave-6a contract:
    //   - the handler's response is field-identical to a fresh
    //     `roster_summary_from_task` projection
    //   - the view is updated by RosterEventSink emit_* so a
    //     just-dispatched task appears without the handler ever
    //     touching the per-task mutex
    //   - the view is updated by RosterEventSink emit_removed so a
    //     pruned task disappears
    //   - the startup rebuild path seeds cold tasks from a
    //     pre-populated store
    #[tokio::test]
    async fn control_roster_view_matches_field_by_field_projection() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::RosterSnapshotV1;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Seed two tasks with distinct inner state so a mis-projection
        // is visible field-for-field.
        let task_a =
            orchestration::test_task("task-a", orchestration::TaskStatus::Running, Provider::Glm);
        {
            let mut inner = task_a.inner.lock();
            inner.bro_label = Some("bro-alpha".to_string());
            inner.managed_worktree = Some("/wt/alpha".to_string());
            inner.cwd = Some("/work/alpha".to_string());
            inner.cost_usd = Some(0.12);
            inner.num_turns = Some(4);
            inner.last_assistant_message = Some("hello".to_string());
            inner.model = Some("glm-pro".to_string());
            inner.origin = bro_core::Origin::Cockpit;
        }
        let task_b = orchestration::test_task(
            "task-b",
            orchestration::TaskStatus::Completed,
            Provider::Deepseek,
        );
        {
            let mut inner = task_b.inner.lock();
            inner.bro_label = Some("bro-beta".to_string());
            inner.managed_worktree = None;
            inner.cwd = None;
            inner.cost_usd = None;
            inner.num_turns = None;
            inner.last_assistant_message = None;
            inner.model = None;
            inner.origin = bro_core::Origin::AgentDispatch;
        }
        {
            let mut store = state.task_store.write();
            store
                .insert("task-a".into(), task_a.clone())
                .expect("insert task-a");
            store
                .insert("task-b".into(), task_b.clone())
                .expect("insert task-b");
        }

        // Drive the view through the same sink path that live ingest
        // uses - emit_added for each task, then a status flip on
        // task-a followed by emit_updated.
        let sink = state.roster_events();
        sink.emit_added(&task_a);
        sink.emit_added(&task_b);
        {
            let mut inner = task_a.inner.lock();
            inner.last_assistant_message = Some("running update".to_string());
        }
        sink.emit_updated(&task_a);

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes)
            .expect("roster body must decode as RosterSnapshotV1");

        let by_id: std::collections::HashMap<_, _> = snapshot
            .tasks
            .iter()
            .map(|s| (s.task_id.as_str().to_string(), s.clone()))
            .collect();
        assert_eq!(by_id.len(), 2);

        // task-a must match the projection byte-for-byte.
        let expected_a = orchestration::roster_summary_from_task(&task_a);
        let served_a = by_id.get("task-a").expect("task-a in snapshot");
        assert_eq!(
            served_a, &expected_a,
            "view must be field-identical to roster_summary_from_task"
        );

        // task-b must match too - covers the no-update, no-aux-fields
        // case.
        let expected_b = orchestration::roster_summary_from_task(&task_b);
        let served_b = by_id.get("task-b").expect("task-b in snapshot");
        assert_eq!(served_b, &expected_b);

        // Sanity: the served a was the post-update projection
        // (snippet == "running update"), proving emit_updated
        // replaced the entry.
        assert_eq!(
            served_a.last_message_snippet.as_deref(),
            Some("running update")
        );
    }

    #[tokio::test]
    async fn control_roster_view_evicts_on_emit_removed() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::RosterSnapshotV1;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        let task = orchestration::test_task(
            "task-evict",
            orchestration::TaskStatus::Running,
            Provider::Glm,
        );
        state
            .task_store
            .write()
            .insert("task-evict".into(), task.clone())
            .expect("insert");

        let sink = state.roster_events();
        sink.emit_added(&task);
        assert_eq!(state.roster_view.snapshot().len(), 1);

        sink.emit_removed("task-evict");

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes).unwrap();
        assert!(
            snapshot.tasks.is_empty(),
            "evicted task must not appear in the served snapshot"
        );
    }

    #[tokio::test]
    async fn control_roster_view_serves_from_seeded_store_on_startup() {
        use axum::body::to_bytes;
        use axum::response::IntoResponse;
        use bro_protocol::RosterSnapshotV1;

        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(SharedState::for_test(tmp.path()));

        // Cold-start: populate the store BEFORE rebuilding the view.
        // (The real daemon does this in the same order: load store
        // from disk, then call `rebuild_from_store`.)
        let task =
            orchestration::test_task("cold", orchestration::TaskStatus::Running, Provider::Glm);
        {
            let mut inner = task.inner.lock();
            inner.bro_label = Some("bro-cold".to_string());
            inner.managed_worktree = Some("/wt/cold".to_string());
        }
        state
            .task_store
            .write()
            .insert("cold".into(), task.clone())
            .expect("insert cold");

        // View is empty until rebuild (for_test does not auto-rebuild).
        assert!(state.roster_view.snapshot().is_empty());

        state
            .roster_view
            .rebuild_from_store(&state.task_store.read());

        let resp = control_roster_handler(AxumState(state.clone()))
            .await
            .into_response();
        let body_bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let snapshot: RosterSnapshotV1 = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(snapshot.tasks.len(), 1);
        let served = &snapshot.tasks[0];
        assert_eq!(served.task_id.as_str(), "cold");
        assert_eq!(served.managed_worktree.as_deref(), Some("/wt/cold"));
        assert_eq!(served.label.as_deref(), Some("bro-cold"));
    }
}

//! Boot-time arc rehydration - replay durable checkpoints written by
//! the runner so suspended arcs survive daemon restarts.
//!
//! Policy (v1):
//! - `Waiting` checkpoints with no in-flight fork dispatches resume:
//!   the runner is reconstructed from the checkpoint and re-enters the
//!   wait node (on_enter skipped), which re-registers the same
//!   correlations into the in-memory WaitStore and replays the
//!   system-events ledger for anything that arrived while down.
//! - Everything else (`Running` mid-node, waits with live forks) is
//!   marked `Interrupted`: node bodies are not idempotent, so silent
//!   re-runs are worse than a loud parked arc. Interrupted arcs surface
//!   through the peek snapshot and a `blocked` note on the arc thread.

use std::sync::Arc;

use anyhow::{Result, anyhow};
use serde_json::json;

use crate::server::state::{ArcSnapshot, BlackboxServer, SharedState};
use crate::workflow::arc_store::{ArcCheckpoint, ArcCheckpointStatus};

use super::{WorkflowRunResult, WorkflowRunner, finish_arc_run};

/// Reconstruct a runner from a checkpoint and drive it to terminal
/// state. The caller owns spawning; this future IS the resumed arc.
pub(crate) async fn resume_workflow_from_checkpoint(
    server: &BlackboxServer,
    cp: ArcCheckpoint,
) -> Result<WorkflowRunResult> {
    let compiled = crate::workflow::compile(cp.workflow.clone())
        .map_err(|e| anyhow!("resume compile for arc {}: {e}", cp.arc_id))?;
    let mut runner = WorkflowRunner::new(
        server,
        &compiled,
        cp.project_dir.clone(),
        cp.max_steps,
        0,
        None,
        Some(cp.arc_id.clone()),
    );
    runner.ctx = cp.ctx;
    runner.node_outputs = cp.node_outputs;
    runner.actor_sessions = cp.actor_sessions;
    runner.ensemble_sessions = cp.ensemble_sessions;
    runner.visit_counts = cp.visit_counts;
    runner.last_verdict = cp.last_verdict;
    runner.steps = cp.steps;
    runner.arc_thread_id = cp.arc_thread_id.clone();
    if cp.status == ArcCheckpointStatus::Waiting {
        runner.resume_skip_on_enter = Some(cp.current_node.clone());
    }
    runner.log_event(
        "rehydrated",
        json!({
            "arc_id": runner.ctx.meta.arc_id,
            "workflow": compiled.spec.name,
            "version": compiled.spec.version,
            "node": cp.current_node,
            "checkpoint_saved_at": cp.saved_at,
            "steps_consumed": cp.steps,
        }),
    );
    runner.arc_note(
        "learned",
        &format!(
            "arc rehydrated after daemon restart; re-entering node '{}' (checkpoint from {})",
            cp.current_node, cp.saved_at
        ),
    );
    runner.update_arc_snapshot("running", "(rehydrated)", Some(&cp.current_node));
    let run_result = runner.run_from(cp.current_node).await;
    Ok(finish_arc_run(runner, run_result).await)
}

/// Boot pass: load every surviving checkpoint, resume the resumable
/// ones as independent tasks, mark the rest interrupted. Never fails
/// the boot; every problem is a warning plus a visible arc state.
pub(crate) async fn rehydrate_arcs(state: Arc<SharedState>) {
    let checkpoints = state.arc_store.load_all().await;
    if checkpoints.is_empty() {
        return;
    }
    let mut resumed = 0usize;
    let mut interrupted = 0usize;
    for cp in checkpoints {
        let resumable =
            cp.status == ArcCheckpointStatus::Waiting && cp.in_flight_nodes.is_empty();
        if resumable {
            resumed += 1;
            let state = state.clone();
            tokio::spawn(async move {
                let arc_id = cp.arc_id.clone();
                let server = BlackboxServer::new(state);
                match resume_workflow_from_checkpoint(&server, cp).await {
                    Ok(res) => {
                        tracing::info!("rehydrated arc {arc_id} reached terminal: {}", res.status);
                    }
                    Err(e) => {
                        tracing::warn!("rehydrated arc {arc_id} failed to resume: {e:#}");
                    }
                }
            });
        } else {
            interrupted += 1;
            mark_arc_interrupted(&state, &cp).await;
        }
    }
    tracing::info!(
        "arc rehydration: {resumed} arc(s) resumed, {interrupted} marked interrupted"
    );
}

async fn mark_arc_interrupted(state: &Arc<SharedState>, cp: &ArcCheckpoint) {
    if let Err(e) = state.arc_store.mark_interrupted(&cp.arc_id).await {
        tracing::warn!("arc {} interrupted-stamp failed: {e:#}", cp.arc_id);
    }
    let reason = if cp.in_flight_nodes.is_empty() {
        format!(
            "interrupted by daemon restart inside node '{}' (non-idempotent body; not auto-resumed)",
            cp.current_node
        )
    } else {
        format!(
            "interrupted by daemon restart at node '{}' with live fork dispatch(es) {:?} lost with the process",
            cp.current_node, cp.in_flight_nodes
        )
    };
    tracing::warn!("arc {} {reason}", cp.arc_id);
    let Some(thread_id) = cp.arc_thread_id.as_deref() else {
        return;
    };
    let mut completed: Vec<String> = cp
        .node_outputs
        .keys()
        .filter(|node_id| cp.workflow.nodes.contains_key(*node_id))
        .cloned()
        .collect();
    completed.sort();
    let now = crate::util::now_iso();
    state.running_arcs.write().insert(
        thread_id.to_string(),
        ArcSnapshot {
            arc_id: cp.arc_id.clone(),
            arc_thread_id: thread_id.to_string(),
            workflow_name: cp.workflow.name.clone(),
            workflow_version: cp.workflow.version,
            status: "interrupted".to_string(),
            current_node: Some(cp.current_node.clone()),
            completed_nodes: completed,
            in_flight_nodes: cp.in_flight_nodes.clone(),
            last_verdict: cp.last_verdict.clone(),
            visit_counts: cp.visit_counts.clone(),
            started_at: cp.ctx.meta.started_at.clone(),
            updated_at: now,
        },
    );
    let params = crate::notes::NoteParams {
        kind: "blocked".into(),
        body: format!("arc {} {reason}", cp.arc_id),
        task_id: None,
        session_id: None,
        project: cp.project_dir.clone(),
        project_id: None,
        thread_id: Some(thread_id.to_string()),
        provider: None,
        bro: None,
    };
    let mut notes = state.notes.write();
    let _ = notes.create(&params);
}

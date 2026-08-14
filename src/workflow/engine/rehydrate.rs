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
    // Process-local claim: two overlapping rehydration passes (or a
    // stray double-call) must not spawn two runners for one arc.
    if !server.state.arc_store.try_claim(&cp.arc_id) {
        return Err(anyhow!(
            "arc {} is already claimed by a live runner in this process",
            cp.arc_id
        ));
    }
    let compiled = match crate::workflow::compile(cp.workflow.clone()) {
        Ok(c) => c,
        Err(e) => {
            server.state.arc_store.release_claim(&cp.arc_id);
            // Release the boot pre-claimed admission key (holder-checked,
            // so this is a no-op unless this arc holds it) and leave a
            // durable interrupted state instead of a live-looking
            // Waiting checkpoint that every future boot retries.
            if let Some(key_map) = cp.ctx.meta.admission_key.as_ref() {
                let canonical = crate::workflow::wait::canonicalize_correlation(key_map);
                server
                    .state
                    .release_arc_admission(&cp.workflow.name, &canonical, &cp.arc_id);
            }
            mark_arc_interrupted(&server.state, &cp).await;
            return Err(anyhow!("resume compile for arc {}: {e}", cp.arc_id));
        }
    };
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
    runner.atom_invocations = cp.atom_invocations;
    runner.visit_counts = cp.visit_counts;
    runner.last_verdict = cp.last_verdict;
    runner.steps = cp.steps;
    runner.arc_thread_id = cp.arc_thread_id.clone();
    if cp.status == ArcCheckpointStatus::Waiting {
        // The Waiting checkpoint was written INSIDE the parked node's
        // step, which run_from will charge again on re-entry; back off
        // by one so resuming does not double-bill the budget (and
        // cannot fail an arc parked exactly at max_steps). Boundary
        // (Running) checkpoints charge the NEXT node fresh and need no
        // adjustment.
        runner.steps = cp.steps.saturating_sub(1);
        runner.resume_skip_on_enter = Some(cp.current_node.clone());
        runner.resume_wait_deadline = cp.waiting_deadline.clone();
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
    // Re-claim the admission key restored with the context; a resumed
    // arc holds the same singleton slot it held before the restart
    // (the boot pass pre-claimed it, so this normally just constructs
    // the lease). A conflict here is defensive: stamp the checkpoint
    // interrupted so it stops looking resumable instead of retrying
    // on every future boot.
    if let Err(e) = runner.claim_admission() {
        runner.arc_note(
            "blocked",
            &format!("rehydration admission re-claim failed: {e}; arc marked interrupted"),
        );
        if let Err(stamp_err) = server.state.arc_store.mark_interrupted(&cp.arc_id).await {
            tracing::warn!(
                "arc {} interrupted-stamp after admission conflict failed: {stamp_err:#}",
                cp.arc_id
            );
        }
        server.state.arc_store.release_claim(&cp.arc_id);
        return Err(e);
    }
    runner.update_arc_snapshot("running", "(rehydrated)", Some(&cp.current_node));
    let arc_id = cp.arc_id.clone();
    let run_result = runner.run_from(cp.current_node).await;
    let result = finish_arc_run(runner, run_result).await;
    server.state.arc_store.release_claim(&arc_id);
    Ok(result)
}

/// Boot pass: load every surviving checkpoint, PRE-CLAIM the admission
/// keys of resumable arcs synchronously (the caller awaits this before
/// the daemon starts serving, so a fresh StartArc can never steal a
/// checkpointed arc's singleton key during the boot window), then
/// resume the claimed arcs as independent tasks and mark the rest
/// interrupted. Never fails the boot; every problem is a warning plus
/// a visible arc state.
pub(crate) async fn rehydrate_arcs(state: Arc<SharedState>) {
    let checkpoints = state.arc_store.load_all().await;
    if checkpoints.is_empty() {
        return;
    }
    let mut resumed = 0usize;
    let mut interrupted = 0usize;
    for cp in checkpoints {
        let resumable = cp.status == ArcCheckpointStatus::Waiting && cp.in_flight_nodes.is_empty();
        if !resumable {
            interrupted += 1;
            mark_arc_interrupted(&state, &cp).await;
            continue;
        }
        // Admission pre-claim, BEFORE any spawn and before the daemon
        // serves: the checkpoint set is the durable truth for who
        // holds a singleton key across a restart. Two checkpoints
        // carrying the same key resolve deterministically here: the
        // first loaded wins, the loser is interrupted instead of
        // silently retrying on every future boot.
        if let Some(key_map) = cp.ctx.meta.admission_key.as_ref() {
            let canonical = crate::workflow::wait::canonicalize_correlation(key_map);
            if let Err(holder) =
                state.claim_arc_admission(&cp.workflow.name, &canonical, &cp.arc_id)
            {
                tracing::warn!(
                    "arc {} duplicate admission key {canonical} (held by {holder}); interrupting instead of resuming",
                    cp.arc_id
                );
                interrupted += 1;
                mark_arc_interrupted(&state, &cp).await;
                continue;
            }
        }
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
    }
    tracing::info!("arc rehydration: {resumed} arc(s) resumed, {interrupted} marked interrupted");
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
            admission_key: cp
                .ctx
                .meta
                .admission_key
                .as_ref()
                .map(crate::workflow::wait::canonicalize_correlation),
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

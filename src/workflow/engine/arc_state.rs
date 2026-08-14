use anyhow::{Result, bail};
use serde_json::json;

use crate::server::state::ArcSnapshot;

use super::WorkflowRunner;

impl WorkflowRunner<'_> {
    /// Open a `bbox_thread(kind=work_item)` for this arc. Silent on failure: an
    /// arc still runs even if thread persistence is unavailable.
    pub(super) fn open_arc_thread(&mut self) {
        let params = crate::threads::ThreadParams {
            action: "open".into(),
            name: Some(format!("wf-{}", self.compiled.spec.name)),
            id: None,
            topic: Some(format!(
                "workflow arc: {} (v{})",
                self.compiled.spec.name, self.compiled.spec.version
            )),
            project: self.project_dir.clone(),
            project_id: None,
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: Some("work_item".into()),
            origin: Some("workflow".into()),
        };
        let result = {
            let mut threads = self.server.state.threads.write();
            threads.thread(&params)
        };
        match result {
            Ok(msg) => {
                let thread_id = msg
                    .split_whitespace()
                    .find(|t| t.starts_with("thread-"))
                    .map(|s| s.to_string());
                if let Some(id) = thread_id {
                    self.log_event(
                        "arc_thread_opened",
                        json!({"thread_id": id, "topic": format!("workflow arc: {}", self.compiled.spec.name)}),
                    );
                    self.arc_thread_id = Some(id);
                }
            }
            Err(e) => {
                self.log_event("arc_thread_open_failed", json!({"error": e.to_string()}));
            }
        }
    }

    fn completed_node_names(&self) -> Vec<String> {
        let mut completed: Vec<String> = self
            .node_outputs
            .keys()
            .filter(|node_id| self.compiled.spec.nodes.contains_key(*node_id))
            .cloned()
            .collect();
        completed.sort();
        completed
    }

    pub(super) fn update_arc_snapshot(&self, status: &str, just_ran: &str, next: Option<&str>) {
        let Some(thread_id) = self.arc_thread_id.as_deref() else {
            return;
        };
        let now = crate::util::now_iso();
        let completed = self.completed_node_names();
        let mut in_flight: Vec<String> = self.in_flight.keys().cloned().collect();
        in_flight.sort();
        let visit_counts: std::collections::HashMap<String, u32> = self
            .visit_counts
            .iter()
            .filter(|(k, _)| !k.starts_with("__"))
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let mut map = self.server.state.running_arcs.write();
        let existing_started = map.get(thread_id).map(|s| s.started_at.clone());
        let snapshot = ArcSnapshot {
            arc_id: self.ctx.meta.arc_id.clone(),
            arc_thread_id: thread_id.to_string(),
            workflow_name: self.compiled.spec.name.clone(),
            workflow_version: self.compiled.spec.version,
            status: status.to_string(),
            current_node: next.map(|s| s.to_string()).or(Some(just_ran.to_string())),
            completed_nodes: completed,
            in_flight_nodes: in_flight,
            last_verdict: self.last_verdict.clone(),
            visit_counts,
            admission_key: self
                .ctx
                .meta
                .admission_key
                .as_ref()
                .map(crate::workflow::wait::canonicalize_correlation),
            started_at: existing_started.unwrap_or_else(|| now.clone()),
            updated_at: now,
        };
        map.insert(thread_id.to_string(), snapshot);
    }

    pub(super) fn arc_note(&self, kind: &str, body: &str) {
        let Some(thread_id) = self.arc_thread_id.as_deref() else {
            return;
        };
        let params = crate::notes::NoteParams {
            kind: kind.into(),
            body: body.into(),
            task_id: None,
            session_id: None,
            project: self.project_dir.clone(),
            project_id: None,
            thread_id: Some(thread_id.to_string()),
            provider: None,
            bro: None,
        };
        let mut notes = self.server.state.notes.write();
        let _ = notes.create(&params);
    }

    pub(super) fn apply_policy_packet(
        &mut self,
        step: usize,
        just_ran: &str,
        next: &str,
    ) -> Result<()> {
        let Some(packet_id) = self.compiled.spec.policy_packet.clone() else {
            return Ok(());
        };
        let completed = self.completed_node_names();
        let mut in_flight: Vec<&String> = self.in_flight.keys().collect();
        in_flight.sort();
        let entity = json!({
            "step": step,
            "just_ran": just_ran,
            "next": next,
            "completed": completed,
            "completed_count": completed.len(),
            "in_flight": in_flight,
            "in_flight_count": in_flight.len(),
            "last_verdict": self.last_verdict,
            "visit_counts": self.visit_counts,
        });
        let verdict = match self.server.apply_workflow_policy(&packet_id, &entity) {
            Ok(v) => v,
            Err(e) => {
                self.log_event("policy_error", json!({"packet_id": packet_id, "error": e}));
                return Ok(());
            }
        };
        let Some(v) = verdict else {
            return Ok(());
        };
        self.log_event(
            "policy_verdict",
            json!({
                "packet_id": packet_id.clone(),
                "verdict": v.clone(),
                "step": step,
            }),
        );
        match v.as_str() {
            "halt" => {
                self.arc_note(
                    "blocked",
                    &format!("policy halt from {packet_id} at step {step} (just_ran={just_ran})"),
                );
                bail!(
                    "policy packet {packet_id} halted arc at step {step} (just_ran='{just_ran}')"
                );
            }
            "escalate" => {
                self.arc_note(
                    "blocked",
                    &format!(
                        "policy escalate from {packet_id} at step {step} (just_ran={just_ran})"
                    ),
                );
            }
            "warn" => {
                self.arc_note(
                    "surprise",
                    &format!("policy warn from {packet_id} at step {step} (just_ran={just_ran})"),
                );
            }
            _ => {}
        }
        Ok(())
    }

    /// Only top-level arcs checkpoint. Sub-workflow runners are not
    /// independently resumable in v1: their parent's Running checkpoint
    /// covers them (rehydration marks it interrupted).
    pub(super) fn should_checkpoint(&self) -> bool {
        self.composition_depth == 0 && self.ctx.meta.parent_arc_id.is_none()
    }

    /// Claim this arc's singleton-admission key, when the workflow
    /// declares one. Fresh arcs derive the key from seeded vars; a
    /// checkpoint resume reuses the restored key verbatim. Failing to
    /// claim is a terminal refusal, not a retryable condition - the
    /// invariant IS "a second arc must not run".
    pub(super) fn claim_admission(&mut self) -> Result<()> {
        if self.composition_depth != 0 || self.ctx.meta.parent_arc_id.is_some() {
            return Ok(());
        }
        let Some(admission) = self.compiled.spec.admission.clone() else {
            return Ok(());
        };
        let key_map = match self.ctx.meta.admission_key.clone() {
            Some(restored) => restored,
            None => {
                let mut m = serde_json::Map::new();
                for k in &admission.key {
                    match self.ctx.vars.get(k) {
                        Some(v) if !v.is_null() => {
                            m.insert(k.clone(), v.clone());
                        }
                        _ => bail!(
                            "admission key var '{k}' missing from initial vars — refusing to admit workflow '{}' (a dedupe key you cannot compute is a routing bug, not a broadcast license)",
                            self.compiled.spec.name
                        ),
                    }
                }
                m
            }
        };
        let canonical = crate::workflow::wait::canonicalize_correlation(&key_map);
        if let Err(holder) = self.server.state.claim_arc_admission(
            &self.compiled.spec.name,
            &canonical,
            &self.ctx.meta.arc_id,
        ) {
            self.arc_note(
                "blocked",
                &format!("duplicate admission refused; key {canonical} held by arc {holder}"),
            );
            bail!(
                "duplicate admission: workflow '{}' key {canonical} is already held by arc {holder}",
                self.compiled.spec.name
            );
        }
        self.ctx.meta.admission_key = Some(key_map);
        self.log_event(
            "admission_claimed",
            serde_json::json!({"workflow": self.compiled.spec.name, "key": canonical}),
        );
        Ok(())
    }

    /// Release the admission key at terminal state (no-op when this arc
    /// holds none, or when a different arc holds the slot).
    pub(super) fn release_admission(&self) {
        if let Some(key_map) = self.ctx.meta.admission_key.as_ref() {
            let canonical = crate::workflow::wait::canonicalize_correlation(key_map);
            self.server.state.release_arc_admission(
                &self.compiled.spec.name,
                &canonical,
                &self.ctx.meta.arc_id,
            );
        }
    }

    pub(super) fn build_checkpoint(
        &self,
        status: crate::workflow::arc_store::ArcCheckpointStatus,
        current_node: &str,
    ) -> crate::workflow::arc_store::ArcCheckpoint {
        self.build_checkpoint_with_deadline(status, current_node, None)
    }

    pub(super) fn build_checkpoint_with_deadline(
        &self,
        status: crate::workflow::arc_store::ArcCheckpointStatus,
        current_node: &str,
        waiting_deadline: Option<String>,
    ) -> crate::workflow::arc_store::ArcCheckpoint {
        let mut in_flight_nodes: Vec<String> = self.in_flight.keys().cloned().collect();
        in_flight_nodes.sort();
        crate::workflow::arc_store::ArcCheckpoint {
            schema_version: crate::workflow::arc_store::ARC_CHECKPOINT_SCHEMA_VERSION,
            arc_id: self.ctx.meta.arc_id.clone(),
            workflow: self.compiled.spec.clone(),
            project_dir: self.project_dir.clone(),
            ctx: self.ctx.clone(),
            node_outputs: self.node_outputs.clone(),
            actor_sessions: self.actor_sessions.clone(),
            ensemble_sessions: self.ensemble_sessions.clone(),
            visit_counts: self.visit_counts.clone(),
            last_verdict: self.last_verdict.clone(),
            steps: self.steps,
            max_steps: self.max_steps,
            current_node: current_node.to_string(),
            status,
            in_flight_nodes,
            arc_thread_id: self.arc_thread_id.clone(),
            waiting_deadline,
            saved_at: crate::util::now_iso(),
        }
    }

    /// Persist the arc's durable position. Failures are logged, never
    /// propagated: losing crash-resume durability must not fail a live
    /// arc that would otherwise run to completion.
    pub(super) async fn write_checkpoint(
        &self,
        status: crate::workflow::arc_store::ArcCheckpointStatus,
        current_node: &str,
    ) {
        self.write_checkpoint_with_deadline(status, current_node, None)
            .await;
    }

    pub(super) async fn write_checkpoint_with_deadline(
        &self,
        status: crate::workflow::arc_store::ArcCheckpointStatus,
        current_node: &str,
        waiting_deadline: Option<String>,
    ) {
        if !self.should_checkpoint() {
            return;
        }
        let cp = self.build_checkpoint_with_deadline(status, current_node, waiting_deadline);
        if let Err(e) = self.server.state.arc_store.save(&cp).await {
            tracing::warn!(
                "arc {} checkpoint write failed at node '{current_node}': {e:#}",
                self.ctx.meta.arc_id
            );
            // A STALE resumable checkpoint is worse than none: if this
            // arc advances past the state on disk and the daemon then
            // restarts, rehydration would re-run already-executed work.
            // Drop durability for this arc instead, and say so on the
            // arc thread.
            if let Err(remove_err) = self
                .server
                .state
                .arc_store
                .remove(&self.ctx.meta.arc_id)
                .await
            {
                tracing::warn!(
                    "arc {} stale-checkpoint removal also failed: {remove_err:#}",
                    self.ctx.meta.arc_id
                );
            }
            self.arc_note(
                "blocked",
                &format!(
                    "arc checkpoint persistence degraded ({e}); crash-resume disabled for this arc"
                ),
            );
        }
    }

    /// Remove the checkpoint at terminal state. Called from the shared
    /// arc epilogue for every terminal outcome (completed, cancelled,
    /// errored): the arc thread keeps the audit trail, and a terminal
    /// arc must not rehydrate.
    pub(super) async fn remove_checkpoint(&self) {
        if !self.should_checkpoint() {
            return;
        }
        if let Err(e) = self
            .server
            .state
            .arc_store
            .remove(&self.ctx.meta.arc_id)
            .await
        {
            tracing::warn!(
                "arc {} checkpoint remove failed: {e:#}",
                self.ctx.meta.arc_id
            );
        }
    }

    pub(super) fn write_compaction_anchor(&self, step: usize, just_ran: &str, next: &str) {
        let completed = self.completed_node_names();
        let mut in_flight: Vec<&String> = self.in_flight.keys().collect();
        in_flight.sort();
        let mut visits: Vec<String> = self
            .visit_counts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        visits.sort();
        let verdict = self.last_verdict.as_deref().unwrap_or("(none)");
        let body = format!(
            "ANCHOR [step {step}, just-ran='{just_ran}', next='{next}']: completed={completed:?} in_flight={in_flight:?} verdict={verdict} visits=[{}]",
            visits.join(", ")
        );
        self.arc_note("learned", &body);
    }
}

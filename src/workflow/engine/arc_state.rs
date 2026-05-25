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

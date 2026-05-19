use crate::notes;
use crate::orchestration;
use crate::orchestration as orch;
use crate::server::state::BlackboxServer;
use crate::threads;
use serde_json::{Value, json};

impl BlackboxServer {
    pub(crate) fn badgey_status_internal(&self, badgey_id: Option<&str>) -> Result<Value, String> {
        if let Some(raw) = badgey_id {
            let id = self.badgey_parse_id(raw)?;
            let instance = self
                .state
                .badgey_registry
                .get_including_dismissed(&id)
                .map_err(|e| e.to_string())?;
            let queue = self
                .state
                .badgey_registry
                .queue_status(&id)
                .map_err(|e| e.to_string())?;
            let proposals = self
                .state
                .badgey_proposals
                .list_by_instance(&id)
                .map_err(|e| format!("listing proposals: {e:#}"))?;
            return Ok(json!({
                "instance": instance,
                "queue": queue,
                "proposals": proposals,
                "observability": self.badgey_observability(&instance),
            }));
        }
        self.badgey_list_internal(false)
    }

    pub(crate) fn badgey_list_internal(&self, include_dismissed: bool) -> Result<Value, String> {
        let instances: Vec<_> = self
            .state
            .badgey_registry
            .list()
            .into_iter()
            .filter(|instance| include_dismissed || !instance.is_dismissed())
            .map(|instance| {
                let queue = self.state.badgey_registry.queue_status(&instance.id).ok();
                json!({
                    "id": instance.id,
                    "scope": instance.scope,
                    "provider": instance.provider,
                    "session_id": instance.provider_session_id,
                    "thread_id": instance.thread_of_record_id,
                    "dismissed": instance.is_dismissed(),
                    "queue": queue,
                })
            })
            .collect();
        Ok(json!({ "instances": instances }))
    }

    pub(crate) fn badgey_collect_internal(
        &self,
        scout_id: Option<&str>,
        badgey_id: Option<&str>,
    ) -> Result<Value, String> {
        let instance = if let Some(raw) = badgey_id {
            let id = self.badgey_parse_id(raw)?;
            Some(
                self.state
                    .badgey_registry
                    .get_including_dismissed(&id)
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let thread_filter = instance.as_ref().map(|i| i.thread_of_record_id.as_str());
        let matching_notes: Vec<_> = self
            .state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| thread_filter.is_none() || note.thread_id.as_deref() == thread_filter)
            .filter(|note| {
                let body = serde_json::from_str::<Value>(&note.body).ok();
                let event = body
                    .as_ref()
                    .and_then(|body| body.get("event"))
                    .and_then(Value::as_str)
                    .unwrap_or("");
                note.kind == notes::NoteKind::Done
                    || matches!(
                        event,
                        "scout_dispatched" | "subbro_spawned" | "scout_done" | "subbro_done"
                    )
                    || event.starts_with("bg-action-spawn-subbro")
            })
            .filter(|note| {
                let body = serde_json::from_str::<Value>(&note.body).unwrap_or_else(
                    |_| json!({"kind": note.kind.clone(), "body": note.body.clone()}),
                );
                scout_id.is_none()
                    || body.get("scout_id").and_then(Value::as_str) == scout_id
                    || body
                        .get("payload")
                        .and_then(|p| p.get("scout_id"))
                        .and_then(Value::as_str)
                        == scout_id
            })
            .cloned()
            .collect();
        let events: Vec<Value> = matching_notes
            .iter()
            .map(|note| {
                serde_json::from_str::<Value>(&note.body).unwrap_or_else(
                    |_| json!({"kind": note.kind.clone(), "body": note.body.clone()}),
                )
            })
            .collect();
        let explicit_aggregate_done = matching_notes.iter().any(|note| {
            serde_json::from_str::<Value>(&note.body)
                .ok()
                .and_then(|body| {
                    body.get("event")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .is_some_and(|event| matches!(event.as_str(), "scout_done" | "subbro_done"))
        });
        let spawned_task_ids: std::collections::HashSet<String> = events
            .iter()
            .filter(|body| body.get("event").and_then(Value::as_str) == Some("subbro_spawned"))
            .filter_map(|body| {
                body.get("task_id")
                    .and_then(Value::as_str)
                    .map(String::from)
            })
            .collect();
        let done_task_ids: std::collections::HashSet<String> = matching_notes
            .iter()
            .filter(|note| note.kind == notes::NoteKind::Done)
            .filter_map(|note| note.task_id.clone())
            .collect();
        let done = explicit_aggregate_done
            || (!spawned_task_ids.is_empty()
                && spawned_task_ids
                    .iter()
                    .all(|task_id| done_task_ids.contains(task_id)))
            || (spawned_task_ids.is_empty()
                && matching_notes
                    .iter()
                    .any(|note| note.kind == notes::NoteKind::Done));
        Ok(json!({
            "status": if done { "done" } else { "still_walking" },
            "scout_id": scout_id,
            "badgey_id": badgey_id,
            "events": events,
        }))
    }

    pub(crate) fn badgey_triage_inbox_internal(
        &self,
        scope: Option<String>,
        since: Option<String>,
        badgey_id: Option<String>,
    ) -> Result<Value, String> {
        let project = scope
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        let stale_threads: Vec<Value> = self
            .state
            .threads
            .read()
            .all()
            .iter()
            .filter(|thread| project.is_empty() || thread.project == project)
            .filter(|thread| {
                since
                    .as_deref()
                    .is_none_or(|since| thread.last_activity.as_str() >= since)
            })
            .filter(|thread| !matches!(thread.status, threads::ThreadStatus::Resolved))
            .take(20)
            .map(|thread| {
                json!({
                    "thread_id": thread.id,
                    "topic": thread.topic,
                    "status": thread.status,
                    "last_activity": thread.last_activity,
                })
            })
            .collect();
        let proposals: Vec<Value> = stale_threads
            .iter()
            .enumerate()
            .map(|(idx, thread)| {
                let stored = badgey_id
                    .as_deref()
                    .and_then(|raw| self.badgey_parse_id(raw).ok())
                    .and_then(|id| {
                        self.state
                            .badgey_proposals
                            .create(
                                &id,
                                orchestration::badgey::types::ProposalKind::RedispatchTask,
                                json!({
                                    "task_id": uuid::Uuid::new_v4().to_string(),
                                    "prompt": format!(
                                        "Review stale work item {} and either close it or issue a narrower follow-up charter.",
                                        thread["thread_id"].as_str().unwrap_or("unknown")
                                    ),
                                    "source_thread_id": thread["thread_id"],
                                    "source": "badgey_triage_inbox",
                                }),
                                thread["thread_id"]
                                    .as_str()
                                    .map(|thread_id| format!("triage:{thread_id}")),
                            )
                            .ok()
                    });
                json!({
                    "id": stored
                        .as_ref()
                        .map(|proposal| proposal.id.clone())
                        .unwrap_or_else(|| format!("triage-{}", idx + 1)),
                    "kind": "redispatch_task",
                    "subject": thread["thread_id"],
                    "proposal": "Review stale work item and either close it or issue a narrower follow-up charter.",
                    "stored": stored.is_some(),
                    "apply_via": badgey_id
                        .as_ref()
                        .map(|id| format!("badgey_resume(id={id:?}, prompt=\"apply P-N\")")),
                })
            })
            .collect();
        Ok(json!({
            "scope": project,
            "since": since,
            "badgey_id": badgey_id,
            "proposal_sheet": {
                "proposals": proposals,
                "source_threads": stale_threads,
            }
        }))
    }

    pub(crate) fn badgey_close_loops_internal(
        &self,
        window_days: Option<u64>,
        project_dir: Option<String>,
    ) -> Result<Value, String> {
        let window_days = window_days.unwrap_or(14);
        let cutoff_ms = orch::now_ms().saturating_sub(window_days.saturating_mul(86_400_000));
        let mut notes = self.state.notes.read();
        let done_task_ids: std::collections::HashSet<String> = notes
            .all()
            .iter()
            .filter(|note| note.kind == notes::NoteKind::Done)
            .filter_map(|note| note.task_id.clone())
            .collect();
        let tasks = self.state.task_store.read().all_tasks();
        let mut classifications = Vec::new();
        for task in tasks {
            let inner = task.inner.lock();
            if project_dir
                .as_deref()
                .is_some_and(|project| inner.cwd.as_deref() != Some(project))
            {
                continue;
            }
            if inner.started_at < cutoff_ms {
                continue;
            }
            if done_task_ids.contains(&inner.id) {
                continue;
            }
            let classification = match inner.status {
                orch::TaskStatus::Failed | orch::TaskStatus::Cancelled => "crashed",
                orch::TaskStatus::Running => "stalled",
                orch::TaskStatus::Completed => "forgot_emit_done",
            };
            if classification == "forgot_emit_done" {
                let already_noted = notes.all().iter().any(|note| {
                    note.kind == notes::NoteKind::Learned
                        && note.task_id.as_deref() == Some(inner.id.as_str())
                        && note.body.contains("closer-suspected-completion")
                });
                if !already_noted {
                    drop(notes);
                    let _ = self.state.notes.write().create(&notes::NoteParams {
                        kind: "learned".to_string(),
                        body: json!({
                            "event": "closer-suspected-completion",
                            "task_id": inner.id.clone(),
                            "contract": "default_completion_contract",
                            "evidence_session": inner.session_id.clone(),
                            "evidence_summary": inner.last_assistant_message.clone(),
                            "synthesized_by": "badgey",
                            "does_not_replace_executor_done": true,
                        })
                        .to_string(),
                        task_id: Some(inner.id.clone()),
                        session_id: Some(inner.session_id.clone()),
                        project: inner.cwd.clone(),
                        thread_id: None,
                        provider: Some(inner.provider.as_str().to_string()),
                        bro: inner.bro_label.clone(),
                    });
                    notes = self.state.notes.read();
                }
            }
            classifications.push(json!({
                "task_id": inner.id,
                "session_id": inner.session_id,
                "provider": inner.provider,
                "classification": classification,
                "does_not_replace_executor_done": true,
            }));
        }
        Ok(json!({
            "window_days": window_days,
            "project_dir": project_dir,
            "classifications": classifications,
            "done_notes_synthesized": 0,
        }))
    }
}

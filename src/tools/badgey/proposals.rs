use std::sync::Arc;

use crate::artifacts::{self, ArtifactInstallParams};
use crate::knowledge;
use crate::notes;
use crate::orchestration;
use crate::orchestration as orch;
use crate::server::progress::{cleanup_policy_file_when_done, resolve_dispatch_filters};
use crate::server::routes::install_artifact_from_params;
use crate::server::state::BlackboxServer;
use crate::threads;
use serde_json::{Map, Value, json};

impl BlackboxServer {
    pub(crate) fn badgey_parse_proposal_kind(
        &self,
        value: &Value,
    ) -> Result<orchestration::badgey::types::ProposalKind, String> {
        let raw = value
            .as_str()
            .ok_or_else(|| "proposal kind must be a string".to_string())?;
        let normalized = match raw.to_ascii_lowercase().replace('-', "_").as_str() {
            "workflow" => "workflow",
            "packet" => "packet",
            "brofile" => "brofile",
            "lens" => "lens",
            "agent" => "agent",
            "redispatch" | "re_dispatch" | "redispatch_task" => "redispatch_task",
            "artifact_promotion" => "artifact_promotion",
            other => return Err(format!("unknown proposal kind: {other}")),
        };
        serde_json::from_value(Value::String(normalized.to_string()))
            .map_err(|e| format!("invalid proposal kind {raw}: {e}"))
    }

    pub(crate) fn badgey_artifact_kind_for_proposal(
        &self,
        kind: orchestration::badgey::types::ProposalKind,
    ) -> Option<artifacts::ArtifactKind> {
        use orchestration::badgey::types::ProposalKind;
        match kind {
            ProposalKind::Workflow => Some(artifacts::ArtifactKind::Workflow),
            ProposalKind::Packet => Some(artifacts::ArtifactKind::Packet),
            ProposalKind::Brofile | ProposalKind::Lens => Some(artifacts::ArtifactKind::Brofile),
            ProposalKind::Agent => Some(artifacts::ArtifactKind::Agent),
            ProposalKind::ArtifactPromotion | ProposalKind::RedispatchTask => None,
        }
    }

    pub(crate) fn badgey_action_result_note(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        action_id: &str,
        event: &str,
        payload: Value,
    ) -> Result<String, String> {
        let mut body = serde_json::Map::new();
        body.insert("event".to_string(), Value::String(event.to_string()));
        body.insert(
            "action_id".to_string(),
            Value::String(action_id.to_string()),
        );
        body.insert("payload".to_string(), payload);
        self.state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind: "learned".to_string(),
                body: Value::Object(body).to_string(),
                task_id: None,
                session_id: Some(instance.provider_session_id.clone()),
                project: Some(instance.scope.project_id.clone()),
                thread_id: Some(instance.thread_of_record_id.clone()),
                provider: Some(instance.provider.as_str().to_string()),
                bro: Some("badgey".to_string()),
            })
            .map_err(|e| format!("writing badgey action result note: {e:#}"))
    }

    pub(crate) fn badgey_next_turn_id(&self, thread_id: &str) -> u64 {
        self.state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| note.thread_id.as_deref() == Some(thread_id))
            .filter_map(|note| {
                serde_json::from_str::<orchestration::badgey::events::ThreadEvent>(&note.body).ok()
            })
            .filter(|event| {
                matches!(
                    event,
                    orchestration::badgey::events::ThreadEvent::Turn { .. }
                )
            })
            .count() as u64
            + 1
    }

    pub(crate) fn badgey_cached_path(
        &self,
        thread_id: &str,
        path_id: &str,
    ) -> Option<orchestration::badgey::events::ThreadEvent> {
        self.state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| note.thread_id.as_deref() == Some(thread_id))
            .filter_map(|note| {
                serde_json::from_str::<orchestration::badgey::events::ThreadEvent>(&note.body).ok()
            })
            .rev()
            .find(|event| {
                matches!(
                    event,
                    orchestration::badgey::events::ThreadEvent::PathCached { id, .. }
                        if id == path_id
                )
            })
    }

    pub(crate) fn badgey_budget_extensions(&self, thread_id: &str) -> u64 {
        self.state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| note.thread_id.as_deref() == Some(thread_id))
            .filter_map(|note| serde_json::from_str::<Value>(&note.body).ok())
            .filter(|body| body.get("event").and_then(Value::as_str) == Some("budget_extended"))
            .count() as u64
    }

    pub(crate) fn badgey_observability(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
    ) -> Value {
        let mut turns = 0u64;
        let mut paths = 0u64;
        let mut scouts = 0u64;
        for note in self.state.notes.read().all() {
            if note.thread_id.as_deref() != Some(instance.thread_of_record_id.as_str()) {
                continue;
            }
            if let Ok(event) =
                serde_json::from_str::<orchestration::badgey::events::ThreadEvent>(&note.body)
            {
                match event {
                    orchestration::badgey::events::ThreadEvent::Turn { .. } => turns += 1,
                    orchestration::badgey::events::ThreadEvent::PathCached { .. } => paths += 1,
                    orchestration::badgey::events::ThreadEvent::SubbroSpawned { .. } => scouts += 1,
                    _ => {}
                }
            }
        }
        let proposals = self
            .state
            .badgey_proposals
            .list_by_instance(&instance.id)
            .unwrap_or_default();
        let applied = proposals
            .iter()
            .filter(|proposal| {
                proposal.state == orchestration::badgey::types::ProposalState::Applied
            })
            .count() as u64;
        let rejected = proposals
            .iter()
            .filter(|proposal| {
                proposal.state == orchestration::badgey::types::ProposalState::Failed
            })
            .count() as u64;
        let total_decided = applied + rejected;
        let accept_rate = if total_decided == 0 {
            None
        } else {
            Some(applied as f64 / total_decided as f64)
        };
        let budget_extensions = self.badgey_budget_extensions(&instance.thread_of_record_id);
        json!({
            "turns": turns,
            "cached_paths": paths,
            "sub_bros": scouts,
            "proposals_total": proposals.len(),
            "proposals_applied": applied,
            "proposals_rejected": rejected,
            "accept_rate": accept_rate,
            "budget": {
                "base_tokens": 50_000,
                "extension_count": budget_extensions,
                "remaining": 50_000 + (budget_extensions * 50_000),
            },
            "learning_loop": {
                "eligible": total_decided >= 5 && accept_rate.unwrap_or(0.0) >= 0.6,
                "reason": "lens proposals remain user-gated; eligibility surfaces for Badgey to draft a brofile/lens proposal"
            }
        })
    }

    pub(crate) fn badgey_refs_consumed_from_result(&self, result: &Value) -> Vec<String> {
        fn collect_strings<'a>(value: &'a Value, out: &mut Vec<&'a str>) {
            match value {
                Value::String(text) => out.push(text),
                Value::Array(items) => {
                    for item in items {
                        collect_strings(item, out);
                    }
                }
                Value::Object(map) => {
                    for value in map.values() {
                        collect_strings(value, out);
                    }
                }
                _ => {}
            }
        }

        let mut refs = Vec::new();
        let mut strings = Vec::new();
        collect_strings(result, &mut strings);
        for text in strings {
            for raw in
                text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ')' | '(' | '[' | ']'))
            {
                let token = raw.trim_matches(|c: char| matches!(c, '"' | '\'' | '.' | ';' | ':'));
                if token.starts_with("knowledge:")
                    || token.starts_with("agent:")
                    || token.starts_with("decision:")
                    || token.starts_with("session:")
                    || token.starts_with("transcript:")
                    || token.starts_with("project_file:")
                    || token.starts_with("symbol:")
                    || token.starts_with("brofile:")
                    || token.starts_with("whiteboard:")
                    || token.starts_with("commit:")
                    || token.starts_with("task:")
                    || token.starts_with("bash_call:")
                    || token.starts_with("domain:")
                    || token.starts_with("artifact:")
                    || token.starts_with("entity:")
                    || token.starts_with("thread-")
                    || token.starts_with("task-")
                    || token.starts_with("note-")
                {
                    let candidate = token.to_string();
                    if !refs.contains(&candidate) {
                        refs.push(candidate);
                    }
                }
                if refs.len() >= 20 {
                    return refs;
                }
            }
        }
        refs
    }

    pub(crate) fn badgey_existing_audit_decision_id(
        &self,
        badgey_id: &str,
        proposal_id: &str,
    ) -> Option<String> {
        let needle = format!("Badgey proposal {proposal_id} for {badgey_id} was applied.");
        self.state
            .kb
            .read()
            .all_entries()
            .iter()
            .find(|entry| entry.content == needle)
            .map(|entry| entry.id.clone())
    }

    pub(crate) async fn badgey_post_process_turn(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        turn_start_iso: &str,
    ) -> Result<Vec<Value>, String> {
        let action_bodies: Vec<Value> = {
            let notes = self.state.notes.read();
            notes
                .all()
                .iter()
                .filter(|note| {
                    note.thread_id.as_deref() == Some(instance.thread_of_record_id.as_str())
                })
                .filter(|note| note.created_at.as_str() >= turn_start_iso)
                .filter_map(|note| serde_json::from_str::<Value>(&note.body).ok())
                .filter(|body| {
                    body.get("event")
                        .and_then(Value::as_str)
                        .is_some_and(|event| {
                            matches!(
                                event,
                                "bg-action-spawn-subbro"
                                    | "bg-action-emit-proposal"
                                    | "bg-action-escalate-dispute"
                                    | "bg-action-extend-budget"
                            )
                        })
                })
                .collect()
        };
        let mut results = Vec::new();
        for body in action_bodies {
            match self.badgey_process_action(instance, body.clone()).await {
                Ok(result) => results.push(result),
                Err(reason) => results.push(self.badgey_fail_action_body(instance, body, reason)?),
            }
        }
        Ok(results)
    }

    pub(crate) fn badgey_fail_action_body(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        body: Value,
        reason: String,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::{ActionId, ActionJournalState};

        let event = body
            .get("event")
            .and_then(Value::as_str)
            .unwrap_or("bg-action-invalid")
            .to_string();
        let action_id_raw = body
            .get("action_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{event} failed without action_id: {reason}"))?
            .to_string();
        let action_id: ActionId = action_id_raw.parse().map_err(|e| {
            format!("invalid action_id {action_id_raw}: {e}; original error: {reason}")
        })?;
        let entry = self
            .state
            .badgey_journal
            .record_seen(action_id.clone(), event.clone(), body)
            .map_err(|e| format!("recording failed action journal: {e}"))?;
        if !entry.state.is_terminal() {
            let _ = self.state.badgey_journal.transition(
                &action_id,
                ActionJournalState::Seen,
                ActionJournalState::Failed {
                    reason: reason.clone(),
                },
                Some("action failed validation or dispatch".to_string()),
            );
        }
        let payload = json!({"reason": reason});
        self.badgey_action_result_note(
            instance,
            &action_id_raw,
            "bg-action-failed",
            payload.clone(),
        )?;
        Ok(json!({
            "action_id": action_id_raw,
            "event": event,
            "status": "failed",
            "payload": payload,
        }))
    }

    pub(crate) async fn badgey_process_action(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        body: Value,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::{ActionId, ActionJournalState};

        let event = body
            .get("event")
            .and_then(Value::as_str)
            .ok_or_else(|| "badgey action missing event".to_string())?
            .to_string();
        let action_id_raw = body
            .get("action_id")
            .and_then(Value::as_str)
            .ok_or_else(|| format!("{event} missing action_id"))?;
        let action_id: ActionId = action_id_raw
            .parse()
            .map_err(|e| format!("invalid action_id {action_id_raw}: {e}"))?;
        let entry = self
            .state
            .badgey_journal
            .record_seen(action_id.clone(), event.clone(), body.clone())
            .map_err(|e| format!("recording action journal: {e}"))?;
        if entry.state.is_terminal() {
            return Ok(json!({
                "action_id": action_id_raw,
                "event": event,
                "status": "already_terminal",
                "state": entry.state,
            }));
        }
        if let ActionJournalState::Dispatching { task_id } = &entry.state {
            if let Some(task) = self.state.task_store.read().get(task_id) {
                let status = task.inner.lock().status;
                if status.is_terminal() {
                    let terminal_state = if status == orch::TaskStatus::Completed {
                        ActionJournalState::Completed {
                            result_ref: format!("task:{task_id}"),
                        }
                    } else {
                        ActionJournalState::Failed {
                            reason: format!("task {task_id} ended with {status:?}"),
                        }
                    };
                    let _ = self.state.badgey_journal.transition(
                        &action_id,
                        entry.state.clone(),
                        terminal_state,
                        Some("reconciled existing dispatch".to_string()),
                    );
                }
            }
            return Ok(json!({
                "action_id": action_id_raw,
                "event": event,
                "status": "dispatching",
                "task_id": task_id,
            }));
        }

        let mut completion_from = ActionJournalState::Seen;
        let dispatch_result = match event.as_str() {
            "bg-action-emit-proposal" => {
                // Accept both `kind` (canonical) and `proposal_kind`
                // (natural LLM shape — synthesis charters describing
                // proposal shape often phrase the field this way).
                let kind_value = body
                    .get("kind")
                    .or_else(|| body.get("proposal_kind"))
                    .ok_or_else(|| {
                        "bg-action-emit-proposal missing kind (or proposal_kind)".to_string()
                    })?;
                let kind = self.badgey_parse_proposal_kind(kind_value)?;
                let idempotency_key = body
                    .get("idempotency_key")
                    .and_then(Value::as_str)
                    .map(String::from)
                    .or_else(|| {
                        (kind == orchestration::badgey::types::ProposalKind::RedispatchTask)
                            .then(|| uuid::Uuid::new_v4().to_string())
                    });
                // Three accepted draft shapes:
                //   1. `draft: {…}` — explicit object (canonical).
                //   2. `proposal: {…}` — explicit object under legacy
                //      alias key (was the original alias path).
                //   3. Top-level structured fields (root_cause /
                //      proposal / blast_radius / draft_artifact_ref /
                //      subject_ref / source / draft_path / task_id) —
                //      synthesized into a draft object. This is the
                //      shape LLMs emit when the synthesis charter
                //      describes those fields directly.
                let proposal_field = body.get("proposal");
                let proposal_is_object = proposal_field.is_some_and(Value::is_object);
                let mut draft = if let Some(d) = body.get("draft") {
                    d.clone()
                } else if proposal_is_object {
                    proposal_field.cloned().unwrap()
                } else {
                    let synthesized: Map<String, Value> = [
                        "headline",
                        "root_cause",
                        "proposal",
                        "blast_radius",
                        "draft_artifact_ref",
                        "subject_ref",
                        "source",
                        "draft_path",
                        "task_id",
                        "name",
                        "version",
                        "supersedes",
                        "evidence_refs",
                    ]
                    .iter()
                    .filter_map(|k| body.get(*k).map(|v| (k.to_string(), v.clone())))
                    .collect();
                    if synthesized.is_empty() {
                        return Err(
                            "bg-action-emit-proposal missing draft (or top-level draft fields)"
                                .to_string(),
                        );
                    }
                    Value::Object(synthesized)
                };
                if kind == orchestration::badgey::types::ProposalKind::RedispatchTask
                    && draft.get("task_id").is_none()
                {
                    if let Some(map) = draft.as_object_mut() {
                        map.insert(
                            "task_id".to_string(),
                            Value::String(uuid::Uuid::new_v4().to_string()),
                        );
                    }
                }
                let proposal = self
                    .state
                    .badgey_proposals
                    .create(&instance.id, kind, draft.clone(), idempotency_key)
                    .map_err(|e| format!("creating badgey proposal: {e}"))?;
                self.badgey_write_event(
                    instance,
                    orchestration::badgey::events::ThreadEvent::ProposalEmitted {
                        proposal_id: proposal.id.clone(),
                        kind,
                        draft_ref: draft
                            .get("source")
                            .or_else(|| draft.get("draft_path"))
                            .and_then(Value::as_str)
                            .unwrap_or("inline-draft")
                            .to_string(),
                        state: proposal.state,
                    },
                    None,
                )?;
                json!({
                    "proposal_id": proposal.id,
                    "kind": kind,
                    "state": proposal.state,
                })
            }
            "bg-action-spawn-subbro" => {
                let charter = body
                    .get("charter")
                    .and_then(Value::as_str)
                    .ok_or_else(|| "bg-action-spawn-subbro missing charter".to_string())?;
                let task_id = uuid::Uuid::new_v4().to_string();
                let dispatching = ActionJournalState::Dispatching {
                    task_id: task_id.clone(),
                };
                self.state
                    .badgey_journal
                    .transition(
                        &action_id,
                        ActionJournalState::Seen,
                        dispatching.clone(),
                        Some("privileged sub-bro dispatch reserved".to_string()),
                    )
                    .map_err(|e| format!("marking action dispatching: {e}"))?;
                completion_from = dispatching.clone();
                if let Err(err) = self.badgey_spawn_privileged_task(
                    &task_id,
                    "badgey-scout-persona",
                    charter,
                    &instance.scope.project_id,
                    Some(instance.thread_of_record_id.as_str()),
                    Some(instance.id.as_str()),
                    Some("badgey-scout".to_string()),
                ) {
                    let _ = self.state.badgey_journal.transition(
                        &action_id,
                        dispatching,
                        ActionJournalState::Failed {
                            reason: err.clone(),
                        },
                        Some("privileged sub-bro dispatch failed".to_string()),
                    );
                    return Err(err);
                }
                self.badgey_write_event(
                    instance,
                    orchestration::badgey::events::ThreadEvent::SubbroSpawned {
                        task_id: task_id.clone(),
                        scout_id: body
                            .get("scout_id")
                            .and_then(Value::as_str)
                            .unwrap_or("scout")
                            .to_string(),
                        charter: charter.to_string(),
                    },
                    Some(task_id.clone()),
                )?;
                json!({"task_id": task_id})
            }
            "bg-action-escalate-dispute" => {
                self.badgey_write_event(
                    instance,
                    orchestration::badgey::events::ThreadEvent::DisputeEscalated {
                        subbro_results: body
                            .get("subbro_results")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default(),
                    },
                    None,
                )?;
                json!({"dispute": "escalated"})
            }
            "bg-action-extend-budget" => {
                json!({"budget": "extended_advisory"})
            }
            _ => return Err(format!("unknown badgey action event: {event}")),
        };

        self.state
            .badgey_journal
            .transition(
                &action_id,
                completion_from,
                ActionJournalState::Completed {
                    result_ref: dispatch_result.to_string(),
                },
                Some("action completed".to_string()),
            )
            .map_err(|e| format!("completing action journal: {e}"))?;
        self.badgey_action_result_note(
            instance,
            action_id_raw,
            "bg-action-completed",
            dispatch_result.clone(),
        )?;
        let mut result = dispatch_result;
        result["action_id"] = Value::String(action_id_raw.to_string());
        result["event"] = Value::String(event);
        result["status"] = Value::String("completed".to_string());
        Ok(result)
    }

    pub(crate) fn badgey_spawn_privileged_task(
        &self,
        task_id: &str,
        brofile: &str,
        prompt: &str,
        project_dir: &str,
        thread_id: Option<&str>,
        work_item_id: Option<&str>,
        label: Option<String>,
    ) -> Result<Arc<orch::Task>, String> {
        let (
            provider,
            lens,
            exec_opts,
            env_overrides,
            cwd,
            brofile_filters,
            _coerce_workspace,
            brofile_context,
        ) = self.resolve_exec_target(Some(brofile), None, Some(project_dir))?;
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            brofile_context.as_ref(),
        );
        let session_id = "pending".to_string();
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.to_string()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(brofile.to_string()),
            thread_id: thread_id.map(String::from),
            work_item_id: work_item_id.map(String::from),
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(brofile),
                Some(session_id.as_str()),
                thread_id,
                work_item_id,
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
            coerce_workspace: false,
        };
        let final_prompt =
            orch::apply_brofile_lens(&orch::apply_ambient(prompt, &ambient_ctx), lens.as_deref());
        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            task_id,
            brofile_filters.as_ref(),
            None,
            &self.state.packets.read(),
        )?;
        args.extend(dispatch_filters.args);
        let task = orch::spawn_with_pre_minted_id(
            task_id.to_string(),
            orch::SpawnTaskParams {
                provider,
                args,
                session_id,
                cwd,
                env_overrides,
                store_dir: self.state.store_dir.clone(),
                task_store: self.state.task_store.clone(),
                tail_tx: self.state.tail_tx.clone(),
                bro_label: label.clone(),
                agent_label: label,
                system_events: Some(self.state.system_events.clone()),
            },
        )
        .map_err(|e| e.to_string())?;
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        Ok(task)
    }

    pub(crate) async fn badgey_apply_proposal_internal(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        proposal_id: &str,
        retry_failed: bool,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::{ProposalKind, ProposalState};

        let instance = self
            .state
            .badgey_registry
            .get(id)
            .map_err(|e| e.to_string())?;
        let proposal = self
            .state
            .badgey_proposals
            .get(id, proposal_id)
            .map_err(|e| format!("reading proposal: {e}"))?
            .ok_or_else(|| format!("error.not_found: proposal {proposal_id}"))?;
        match proposal.state {
            ProposalState::Applied => {
                return Ok(json!({
                    "badgey_id": id,
                    "proposal_id": proposal_id,
                    "already_applied": true,
                    "prior_task_id": proposal.applied_task_id,
                }));
            }
            ProposalState::Applying => {
                return Err("error.bad_input(code=already_in_progress)".to_string());
            }
            ProposalState::Failed if !retry_failed => {
                return Err(format!(
                    "error.bad_input(code=proposal_failed): retry with `retry apply {proposal_id}`"
                ));
            }
            ProposalState::Pending | ProposalState::Failed => {}
        }
        let from = proposal.state;
        let applying = self
            .state
            .badgey_proposals
            .transition(
                id,
                proposal_id,
                from,
                ProposalState::Applying,
                Some(if retry_failed {
                    "retry apply requested".to_string()
                } else {
                    "apply requested".to_string()
                }),
            )
            .map_err(|e| format!("transitioning proposal to applying: {e}"))?;

        let apply_result = async {
            if let Some(kind) = self.badgey_artifact_kind_for_proposal(applying.kind) {
                let source = applying
                    .draft
                    .get("source")
                    .or_else(|| applying.draft.get("draft_path"))
                    .or_else(|| applying.draft.get("path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "artifact proposal draft missing source/draft_path".to_string()
                    })?;
                let metadata = install_artifact_from_params(
                    &self.state,
                    ArtifactInstallParams {
                        kind,
                        source: source.to_string(),
                        name: applying
                            .draft
                            .get("name")
                            .and_then(Value::as_str)
                            .map(String::from),
                        version: applying
                            .draft
                            .get("version")
                            .and_then(Value::as_str)
                            .map(String::from),
                        supersedes: applying
                            .draft
                            .get("supersedes")
                            .and_then(Value::as_str)
                            .map(String::from),
                    },
                )
                .await
                .map_err(|e| format!("installing artifact proposal: {e:#}"))?;
                Ok(json!({
                    "artifact_ref": format!("{:?}:{}@{}", kind, metadata.name, metadata.version),
                    "metadata": metadata,
                }))
            } else if applying.kind == ProposalKind::RedispatchTask {
                // Accept the canonical fields plus Badgey's natural
                // emission shape: synthesis charters describe the
                // human-readable action under `proposal`, which is
                // what we want as the dispatch prompt for a redispatch.
                let prompt = applying
                    .draft
                    .get("prompt")
                    .or_else(|| applying.draft.get("refined_charter"))
                    .or_else(|| applying.draft.get("proposal"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "redispatch proposal missing prompt/refined_charter/proposal".to_string()
                    })?;
                if applying.idempotency_key.is_none() {
                    return Err("redispatch proposal missing idempotency_key".to_string());
                }
                let task_id = applying
                    .applied_task_id
                    .clone()
                    .or_else(|| {
                        applying
                            .draft
                            .get("task_id")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
                self.state
                    .badgey_proposals
                    .set_applied_task_id(id, proposal_id, task_id.clone())
                    .map_err(|e| format!("recording redispatch task id: {e}"))?;
                self.badgey_spawn_privileged_task(
                    &task_id,
                    "badgey-persona",
                    prompt,
                    &instance.scope.project_id,
                    Some(instance.thread_of_record_id.as_str()),
                    Some(id.as_str()),
                    Some("badgey-redispatch".to_string()),
                )?;
                Ok(json!({"task_id": task_id}))
            } else {
                let kind = applying
                    .draft
                    .get("artifact_kind")
                    .or_else(|| applying.draft.get("kind"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| "artifact promotion draft missing artifact_kind".to_string())
                    .and_then(|raw| match raw {
                        "workflow" => Ok(artifacts::ArtifactKind::Workflow),
                        "packet" => Ok(artifacts::ArtifactKind::Packet),
                        "brofile" => Ok(artifacts::ArtifactKind::Brofile),
                        "agent" => Ok(artifacts::ArtifactKind::Agent),
                        other => Err(format!("unknown artifact promotion kind: {other}")),
                    })?;
                let source = applying
                    .draft
                    .get("source")
                    .or_else(|| applying.draft.get("draft_path"))
                    .or_else(|| applying.draft.get("path"))
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        "artifact promotion draft missing source/draft_path".to_string()
                    })?;
                let metadata = install_artifact_from_params(
                    &self.state,
                    ArtifactInstallParams {
                        kind,
                        source: source.to_string(),
                        name: applying
                            .draft
                            .get("name")
                            .and_then(Value::as_str)
                            .map(String::from),
                        version: applying
                            .draft
                            .get("version")
                            .and_then(Value::as_str)
                            .map(String::from),
                        supersedes: applying
                            .draft
                            .get("supersedes")
                            .and_then(Value::as_str)
                            .map(String::from),
                    },
                )
                .await
                .map_err(|e| format!("promoting artifact proposal: {e:#}"))?;
                Ok(json!({
                    "artifact_ref": format!("{:?}:{}@{}", kind, metadata.name, metadata.version),
                    "metadata": metadata,
                }))
            }
        }
        .await;

        match apply_result {
            Ok(outcome) => {
                let applied = self
                    .state
                    .badgey_proposals
                    .transition(
                        id,
                        proposal_id,
                        ProposalState::Applying,
                        ProposalState::Applied,
                        Some(outcome.to_string()),
                    )
                    .map_err(|e| format!("transitioning proposal to applied: {e}"))?;
                let decide_id = if let Some(existing) =
                    self.badgey_existing_audit_decision_id(id.as_str(), proposal_id)
                {
                    existing
                } else {
                    self.state
                        .kb
                        .write()
                        .decide_result(
                            &knowledge::DecideParams {
                                content: format!(
                                    "Badgey proposal {proposal_id} for {id} was applied."
                                ),
                                rationale: format!("User approved Badgey proposal {proposal_id}."),
                                supersedes: applying
                                    .draft
                                    .get("audit_supersedes")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                title: Some(format!("Badgey proposal {proposal_id} applied")),
                                scope: Some("project".to_string()),
                                project: Some(instance.scope.project_id.clone()),
                                priority: Some("standard".to_string()),
                                render: Some(false),
                            },
                            false,
                        )
                        .map_err(|e| format!("writing proposal audit decision: {e:#}"))?
                        .id
                };
                let artifact_ref = outcome
                    .get("artifact_ref")
                    .and_then(Value::as_str)
                    .unwrap_or("task")
                    .to_string();
                self.badgey_write_event(
                    &instance,
                    orchestration::badgey::events::ThreadEvent::ProposalApplied {
                        proposal_id: proposal_id.to_string(),
                        artifact_ref,
                        decide_id: decide_id.clone(),
                    },
                    applied.applied_task_id.clone(),
                )?;
                Ok(json!({
                    "badgey_id": id,
                    "proposal_id": proposal_id,
                    "status": "applied",
                    "proposal": applied,
                    "outcome": outcome,
                    "decide_id": decide_id,
                }))
            }
            Err(err) => {
                let _ = self.state.badgey_proposals.transition(
                    id,
                    proposal_id,
                    ProposalState::Applying,
                    ProposalState::Failed,
                    Some(err.clone()),
                );
                Err(err)
            }
        }
    }

    /// Begin the apply path for a proposal: transition Pending|Failed →
    /// Applying, return dispatch parameters that the caller (a workflow
    /// arc) uses to actually do the work via an actor node or
    /// mcp_call. Pairs with [`badgey_proposal_complete_apply_internal`].
    ///
    /// Return shape — flat object the workflow can destructure into
    /// vars in one set_var per field:
    ///
    /// Pre-existing terminal states:
    /// - `{outcome: "already_applied", prior_task_id?: "..."}` — proposal
    ///   was already in Applied state; caller should skip dispatch and
    ///   skip the complete call. PostOutcome emits the green badge.
    /// - `{outcome: "rejected", reason: "..."}` — bad-input shape (e.g.
    ///   already_in_progress, failed-without-retry).
    ///
    /// Ready-to-dispatch states:
    /// - `{outcome: "redispatch", kind: "redispatch_task", prompt, task_id,
    ///    instance_id, project_dir, brofile, label, idempotency_key}` —
    ///   caller dispatches a Claude actor with `prompt`.
    /// - `{outcome: "install", kind: "artifact_promotion"|...,
    ///    artifact_kind: "workflow"|"packet"|..., source, name?,
    ///    version?, supersedes?, instance_id, project_dir}` — caller
    ///   does an `mcp_call bbox_artifact_install`.
    pub(crate) async fn badgey_proposal_begin_apply_internal(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        proposal_id: &str,
        retry_failed: bool,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::{ProposalKind, ProposalState};

        let instance = self
            .state
            .badgey_registry
            .get(id)
            .map_err(|e| e.to_string())?;
        let proposal = self
            .state
            .badgey_proposals
            .get(id, proposal_id)
            .map_err(|e| format!("reading proposal: {e}"))?
            .ok_or_else(|| format!("error.not_found: proposal {proposal_id}"))?;
        match proposal.state {
            ProposalState::Applied => {
                let prior = proposal.applied_task_id.clone().unwrap_or_default();
                let summary = if prior.is_empty() {
                    "already applied".to_string()
                } else {
                    format!("already applied (prior task `{prior}`)")
                };
                return Ok(json!({
                    "outcome": "already_applied",
                    "badgey_id": id,
                    "proposal_id": proposal_id,
                    "prior_task_id": proposal.applied_task_id,
                    "summary": summary,
                }));
            }
            ProposalState::Applying => {
                return Ok(json!({
                    "outcome": "rejected",
                    "reason": "already_in_progress",
                    "badgey_id": id,
                    "proposal_id": proposal_id,
                    "summary": "rejected: already in progress",
                }));
            }
            ProposalState::Failed if !retry_failed => {
                return Ok(json!({
                    "outcome": "rejected",
                    "reason": "proposal_failed",
                    "hint": format!("retry with retry_failed=true on proposal {proposal_id}"),
                    "badgey_id": id,
                    "proposal_id": proposal_id,
                    "summary": format!(
                        "rejected: proposal previously failed — retry with `retry_failed=true`"
                    ),
                }));
            }
            ProposalState::Pending | ProposalState::Failed => {}
        }
        let from = proposal.state;
        let applying = self
            .state
            .badgey_proposals
            .transition(
                id,
                proposal_id,
                from,
                ProposalState::Applying,
                Some(if retry_failed {
                    "retry apply requested".to_string()
                } else {
                    "apply requested".to_string()
                }),
            )
            .map_err(|e| format!("transitioning proposal to applying: {e}"))?;

        if applying.kind == ProposalKind::RedispatchTask {
            let prompt = applying
                .draft
                .get("prompt")
                .or_else(|| applying.draft.get("refined_charter"))
                .or_else(|| applying.draft.get("proposal"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    "redispatch proposal missing prompt/refined_charter/proposal".to_string()
                })?;
            if applying.idempotency_key.is_none() {
                return Err("redispatch proposal missing idempotency_key".to_string());
            }
            let task_id = applying
                .applied_task_id
                .clone()
                .or_else(|| {
                    applying
                        .draft
                        .get("task_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                })
                .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
            self.state
                .badgey_proposals
                .set_applied_task_id(id, proposal_id, task_id.clone())
                .map_err(|e| format!("recording redispatch task id: {e}"))?;
            return Ok(json!({
                "outcome": "redispatch",
                "kind": "redispatch_task",
                "prompt": prompt,
                "task_id": task_id,
                "instance_id": id.as_str(),
                "proposal_id": proposal_id,
                "project_dir": instance.scope.project_id,
                "thread_id": instance.thread_of_record_id,
                "brofile": "badgey-persona",
                "label": "badgey-redispatch",
                "idempotency_key": applying.idempotency_key,
                "summary": format!("dispatching task `{task_id}`..."),
            }));
        }
        // Artifact-install kinds — return install params; caller
        // mcp_calls bbox_artifact_install. The artifact_kind comes from
        // the proposal kind itself for direct kinds (workflow / packet /
        // brofile / lens / agent), or from draft.artifact_kind for
        // generic ArtifactPromotion proposals.
        let artifact_kind_str = match applying.kind {
            ProposalKind::Workflow => "workflow",
            ProposalKind::Packet => "packet",
            ProposalKind::Brofile | ProposalKind::Lens => "brofile",
            ProposalKind::Agent => "agent",
            ProposalKind::ArtifactPromotion => applying
                .draft
                .get("artifact_kind")
                .or_else(|| applying.draft.get("kind"))
                .and_then(Value::as_str)
                .ok_or_else(|| "artifact promotion draft missing artifact_kind".to_string())?,
            ProposalKind::RedispatchTask => unreachable!("handled above"),
        };
        let source = applying
            .draft
            .get("source")
            .or_else(|| applying.draft.get("draft_path"))
            .or_else(|| applying.draft.get("path"))
            .and_then(Value::as_str)
            .ok_or_else(|| "artifact proposal draft missing source/draft_path".to_string())?;
        Ok(json!({
            "outcome": "install",
            "kind": format!("{:?}", applying.kind).to_lowercase(),
            "artifact_kind": artifact_kind_str,
            "source": source,
            "name": applying.draft.get("name"),
            "version": applying.draft.get("version"),
            "supersedes": applying.draft.get("supersedes"),
            "instance_id": id.as_str(),
            "proposal_id": proposal_id,
            "project_dir": instance.scope.project_id,
            "summary": format!("installing {artifact_kind_str} from `{source}`..."),
        }))
    }

    /// Complete the apply path: transition Applying → Applied (on
    /// success) or Applying → Failed (on any non-success outcome),
    /// write the audit decision, emit the ProposalApplied event.
    /// Pairs with [`badgey_proposal_begin_apply_internal`].
    ///
    /// `outcome` values: `completed` (success) → Applied; anything else
    /// (`failed`, `cancelled`, `timed_out`) → Failed.
    pub(crate) async fn badgey_proposal_complete_apply_internal(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        proposal_id: &str,
        outcome: &str,
        task_id: Option<&str>,
        artifact_ref: Option<&str>,
        summary: Option<&str>,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::ProposalState;

        let instance = self
            .state
            .badgey_registry
            .get(id)
            .map_err(|e| e.to_string())?;
        let success = outcome == "completed";
        if success {
            let note = match (artifact_ref, task_id, summary) {
                (Some(ar), _, _) => json!({"artifact_ref": ar, "summary": summary}).to_string(),
                (None, Some(tid), _) => json!({"task_id": tid, "summary": summary}).to_string(),
                _ => json!({"summary": summary}).to_string(),
            };
            let applied = self
                .state
                .badgey_proposals
                .transition(
                    id,
                    proposal_id,
                    ProposalState::Applying,
                    ProposalState::Applied,
                    Some(note),
                )
                .map_err(|e| format!("transitioning proposal to applied: {e}"))?;
            let decide_id = if let Some(existing) =
                self.badgey_existing_audit_decision_id(id.as_str(), proposal_id)
            {
                existing
            } else {
                self.state
                    .kb
                    .write()
                    .decide_result(
                        &knowledge::DecideParams {
                            content: format!("Badgey proposal {proposal_id} for {id} was applied."),
                            rationale: format!("User approved Badgey proposal {proposal_id}."),
                            supersedes: applied
                                .draft
                                .get("audit_supersedes")
                                .and_then(Value::as_str)
                                .map(String::from),
                            title: Some(format!("Badgey proposal {proposal_id} applied")),
                            scope: Some("project".to_string()),
                            project: Some(instance.scope.project_id.clone()),
                            priority: Some("standard".to_string()),
                            render: Some(false),
                        },
                        false,
                    )
                    .map_err(|e| format!("writing proposal audit decision: {e:#}"))?
                    .id
            };
            let audit_ref = artifact_ref
                .map(String::from)
                .unwrap_or_else(|| "task".to_string());
            self.badgey_write_event(
                &instance,
                orchestration::badgey::events::ThreadEvent::ProposalApplied {
                    proposal_id: proposal_id.to_string(),
                    artifact_ref: audit_ref,
                    decide_id: decide_id.clone(),
                },
                applied.applied_task_id.clone(),
            )?;
            Ok(json!({
                "status": "applied",
                "badgey_id": id,
                "proposal_id": proposal_id,
                "task_id": task_id,
                "artifact_ref": artifact_ref,
                "summary": summary,
                "decide_id": decide_id,
            }))
        } else {
            let err_note = format!(
                "actor outcome={outcome}; {}",
                summary.unwrap_or("no summary")
            );
            let _ = self.state.badgey_proposals.transition(
                id,
                proposal_id,
                ProposalState::Applying,
                ProposalState::Failed,
                Some(err_note.clone()),
            );
            Ok(json!({
                "status": "failed",
                "badgey_id": id,
                "proposal_id": proposal_id,
                "outcome": outcome,
                "summary": summary,
                "error": err_note,
            }))
        }
    }

    pub(crate) fn badgey_reject_proposal_internal(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        proposal_id: &str,
    ) -> Result<Value, String> {
        use orchestration::badgey::types::ProposalState;

        let instance = self
            .state
            .badgey_registry
            .get(id)
            .map_err(|e| e.to_string())?;
        let current = self
            .state
            .badgey_proposals
            .get(id, proposal_id)
            .map_err(|e| format!("reading proposal: {e}"))?
            .ok_or_else(|| format!("error.not_found: proposal {proposal_id}"))?;
        if current.state == ProposalState::Applied {
            return Err("error.bad_input(code=already_applied)".to_string());
        }
        if current.state == ProposalState::Failed {
            return Ok(json!({
                "badgey_id": id,
                "proposal_id": proposal_id,
                "status": "already_rejected",
            }));
        }
        let rejected = self
            .state
            .badgey_proposals
            .transition(
                id,
                proposal_id,
                current.state,
                ProposalState::Failed,
                Some("rejected by user".to_string()),
            )
            .map_err(|e| format!("rejecting proposal: {e}"))?;
        self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::ProposalRejected {
                proposal_id: proposal_id.to_string(),
                reason: "rejected by user".to_string(),
            },
            None,
        )?;
        Ok(json!({
            "badgey_id": id,
            "proposal_id": proposal_id,
            "status": "rejected",
            "proposal": rejected,
        }))
    }

    pub(crate) fn badgey_dismiss_internal(
        &self,
        badgey_id: &str,
        reason: Option<String>,
    ) -> Result<Value, String> {
        let id = self.badgey_parse_id(badgey_id)?;
        let instance = self
            .state
            .badgey_registry
            .dismiss(&id)
            .map_err(|e| e.to_string())?;
        let reason = reason.unwrap_or_else(|| "dismissed by caller".to_string());
        self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::Dismiss {
                reason: reason.clone(),
                summary: "Badgey instance dismissed; pending resume queue drained.".to_string(),
            },
            None,
        )?;
        let _ = self.state.threads.write().thread(&threads::ThreadParams {
            action: "resolve".to_string(),
            name: None,
            id: Some(instance.thread_of_record_id.clone()),
            topic: None,
            project: None,
            session_id: None,
            provider: None,
            session_name: None,
            handoff_doc: None,
            note: Some(reason),
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
            origin: None,
        });
        Ok(json!({
            "badgey_id": id,
            "status": "dismissed",
            "thread_id": instance.thread_of_record_id,
        }))
    }
}

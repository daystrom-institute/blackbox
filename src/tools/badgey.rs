#![allow(clippy::too_many_arguments)]

use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::badgey_tools()
}

#[tool_router(router = badgey_tools)]
impl BlackboxServer {
    pub(crate) fn badgey_parse_id(
        &self,
        raw: &str,
    ) -> Result<orchestration::badgey::types::BadgeyId, String> {
        raw.parse()
            .map_err(|e: String| format!("error.bad_input(code=invalid_badgey_id): {e}"))
    }

    pub(crate) fn badgey_thread_id_from_open_result(&self, result: &str) -> Result<String, String> {
        let re = regex::Regex::new(r"Thread created: (thread-[0-9a-f]{8})")
            .map_err(|e| format!("internal regex error: {e}"))?;
        re.captures(result)
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
            .ok_or_else(|| format!("could not parse thread id from bbox_thread result: {result}"))
    }

    pub(crate) fn badgey_scope_bind(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        thread_id: &str,
        scope: &orchestration::badgey::types::BadgeyScope,
    ) -> String {
        let brief = scope
            .initial_brief
            .as_deref()
            .unwrap_or("general consultation");
        let recent_proposals = self
            .state
            .badgey_proposals
            .list_by_instance(id)
            .map(|proposals| {
                proposals
                    .into_iter()
                    .rev()
                    .take(8)
                    .map(|p| format!("{}:{:?}:{:?}", p.id, p.kind, p.state))
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        let queue_status = self
            .state
            .badgey_registry
            .queue_status(id)
            .ok()
            .and_then(|status| serde_json::to_string(&status).ok())
            .unwrap_or_else(|| "unregistered".to_string());
        let recent_paths = self
            .state
            .notes
            .read()
            .all()
            .iter()
            .filter(|note| note.thread_id.as_deref() == Some(thread_id))
            .filter_map(|note| {
                serde_json::from_str::<orchestration::badgey::events::ThreadEvent>(&note.body).ok()
            })
            .filter_map(|event| match event {
                orchestration::badgey::events::ThreadEvent::PathCached { id, summary, .. } => {
                    Some(format!("{id}:{summary}"))
                }
                _ => None,
            })
            .rev()
            .take(5)
            .collect::<Vec<_>>()
            .join(", ");
        let budget_extensions = self.badgey_budget_extensions(thread_id);
        let budget_remaining = 50_000 + (budget_extensions * 50_000);
        format!(
            "[badgey-scope]\nbadgey_id: {id}\nthread_of_record: {thread_id}\nproject: {project}\ncurrent_time: {current_time}\nbrief: {brief}\nqueue: {queue_status}\nrecent_paths: {recent_paths}\nrecent_proposals: {recent_proposals}\nbudget_remaining: {budget_remaining}\n[/badgey-scope]\n",
            current_time = util::now_iso(),
            project = scope.project_id
        )
    }

    pub(crate) fn badgey_write_event(
        &self,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        event: orchestration::badgey::events::ThreadEvent,
        task_id: Option<String>,
    ) -> Result<String, String> {
        let kind = event.note_kind().to_string();
        let body = serde_json::to_string(&event)
            .map_err(|e| format!("serializing badgey thread event: {e}"))?;
        self.state
            .notes
            .write()
            .create(&notes::NoteParams {
                kind,
                body,
                task_id,
                session_id: Some(instance.provider_session_id.clone()),
                project: Some(instance.scope.project_id.clone()),
                thread_id: Some(instance.thread_of_record_id.clone()),
                provider: Some(instance.provider.as_str().to_string()),
                bro: Some("badgey".to_string()),
            })
            .map_err(|e| format!("writing badgey thread event: {e:#}"))
    }

    pub(crate) fn badgey_launch_exec(
        &self,
        id: &orchestration::badgey::types::BadgeyId,
        scope: &orchestration::badgey::types::BadgeyScope,
        thread_id: &str,
        bro_label: Option<String>,
    ) -> Result<
        (
            Arc<orch::Task>,
            Provider,
            String,
            orchestration::mcp::McpFilters,
        ),
        String,
    > {
        let store_dir = self.state.store_dir.clone();
        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters) =
            self.resolve_exec_target(Some("badgey-persona"), None, Some(&scope.project_id))?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = "pending".to_string();
        let scope_bind = self.badgey_scope_bind(id, thread_id, scope);
        let prompt = format!(
            "{}\nInitialize this Badgey consultation and answer the initial brief. Keep all durable observations in the thread of record.\n",
            scope_bind
        );
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some("badgey-persona".to_string()),
            thread_id: Some(thread_id.to_string()),
            work_item_id: Some(id.as_str().to_string()),
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some("badgey-persona"),
                Some(session_id.as_str()),
                Some(thread_id),
                Some(id.as_str()),
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
        };
        let final_prompt =
            orch::apply_brofile_lens(&orch::apply_ambient(&prompt, &ambient_ctx), lens.as_deref());
        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let filters = brofile_filters.unwrap_or_default();
        let dispatch_filters = resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            &task_id,
            Some(&filters),
            None,
            &self.state.packets.read(),
        )?;
        let effective_filters = dispatch_filters.filters.clone();
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id.clone(),
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            bro_label.clone(),
            bro_label,
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        Ok((task, provider, session_id, effective_filters))
    }

    pub(crate) async fn badgey_wait_for_observed_session_id(
        &self,
        task: &Arc<orch::Task>,
        timeout_seconds: f64,
    ) -> Result<String, String> {
        let wait = async {
            loop {
                {
                    let inner = task.inner.lock();
                    if inner.session_id != "pending" {
                        return Ok(inner.session_id.clone());
                    }
                    if inner.status.is_terminal() {
                        return Err(format!(
                            "provider session id was not observed before task reached {:?}",
                            inner.status
                        ));
                    }
                }
                tokio::select! {
                    _ = task.notify.notified() => {}
                    _ = tokio::time::sleep(std::time::Duration::from_millis(100)) => {}
                }
            }
        };
        tokio::time::timeout(std::time::Duration::from_secs_f64(timeout_seconds), wait)
            .await
            .map_err(|_| {
                "provider session id was not observed before Badgey registration timeout"
                    .to_string()
            })?
    }

    pub(crate) async fn badgey_exec_internal(
        &self,
        project_dir: Option<String>,
        brief: Option<String>,
        bro_label: Option<String>,
    ) -> Result<Value, String> {
        let project_id = project_dir
            .or_else(|| {
                std::env::current_dir()
                    .ok()
                    .map(|p| p.to_string_lossy().to_string())
            })
            .unwrap_or_default();
        let id = orchestration::badgey::types::BadgeyId::new();
        let scope = orchestration::badgey::types::BadgeyScope {
            project_id: project_id.clone(),
            initial_brief: brief.clone(),
        };
        let thread_result = self
            .state
            .threads
            .write()
            .thread(&threads::ThreadParams {
                action: "open".to_string(),
                name: Some(format!("badgey:{}", id.as_str())),
                id: None,
                topic: Some(format!(
                    "Badgey consultation: {}",
                    brief.as_deref().unwrap_or("general consultation")
                )),
                project: Some(project_id.clone()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: Some("Badgey thread of record".to_string()),
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".to_string()),
            })
            .map_err(|e| format!("opening badgey thread of record: {e:#}"))?;
        let thread_id = self.badgey_thread_id_from_open_result(&thread_result)?;
        let (task, provider, _initial_session_id, merged_filters) =
            self.badgey_launch_exec(&id, &scope, &thread_id, bro_label)?;
        let task_id = task.inner.lock().id.clone();
        let session_id = match self.badgey_wait_for_observed_session_id(&task, 10.0).await {
            Ok(session_id) => session_id,
            Err(err) => {
                let _ = self.state.notes.write().create(&notes::NoteParams {
                    kind: "surprise".to_string(),
                    body: json!({
                        "event": "badgey_exec_unobserved_session",
                        "badgey_id": id,
                        "task_id": task_id,
                        "reason": err,
                    })
                    .to_string(),
                    task_id: Some(task_id),
                    session_id: None,
                    project: Some(project_id),
                    thread_id: Some(thread_id),
                    provider: Some(provider.as_str().to_string()),
                    bro: Some("badgey".to_string()),
                });
                return Err(err);
            }
        };
        let instance = orchestration::badgey::registry::BadgeyInstance::new(
            id.clone(),
            scope.clone(),
            provider,
            session_id.clone(),
            thread_id.clone(),
        );
        self.state
            .badgey_registry
            .register(instance.clone())
            .map_err(|e| e.to_string())?;
        let _ = self.state.threads.write().thread(&threads::ThreadParams {
            action: "continue".to_string(),
            name: None,
            id: Some(thread_id.clone()),
            topic: None,
            project: None,
            session_id: Some(session_id.clone()),
            provider: Some(provider.as_str().to_string()),
            session_name: Some("badgey".to_string()),
            handoff_doc: None,
            note: None,
            target: None,
            target_type: None,
            edge: None,
            promoted_to: None,
            kind: None,
        });
        self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::Exec {
                brofile_version: "badgey-persona".to_string(),
                scope,
                charter: brief.unwrap_or_else(|| "general consultation".to_string()),
                provider,
                provider_session_id: session_id.clone(),
            },
            Some(task_id.clone()),
        )?;
        Ok(json!({
            "badgey_id": id,
            "task_id": task_id,
            "session_id": session_id,
            "provider": provider,
            "thread_id": thread_id,
            "status": "running",
            "resolved_brofile": "badgey-persona",
            "merged_filters": merged_filters,
        }))
    }

    pub(crate) async fn badgey_resume_internal(
        &self,
        badgey_id: &str,
        prompt: &str,
        timeout_seconds: Option<f64>,
    ) -> Result<Value, String> {
        use orchestration::badgey::commands::{WrapperCommand, parse_command};

        let id = self.badgey_parse_id(badgey_id)?;
        match parse_command(prompt) {
            Some(WrapperCommand::Dismiss) => {
                return self
                    .badgey_dismiss_internal(badgey_id, Some("wrapper command".to_string()));
            }
            Some(WrapperCommand::ApplyProposal(proposal_id)) => {
                return self
                    .badgey_apply_proposal_internal(&id, &proposal_id, false)
                    .await;
            }
            Some(WrapperCommand::RetryApply(proposal_id)) => {
                return self
                    .badgey_apply_proposal_internal(&id, &proposal_id, true)
                    .await;
            }
            Some(WrapperCommand::RejectProposal(proposal_id)) => {
                return self.badgey_reject_proposal_internal(&id, &proposal_id);
            }
            Some(WrapperCommand::ExpandPath(path_id)) => {
                let instance = self
                    .state
                    .badgey_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                return match self.badgey_cached_path(&instance.thread_of_record_id, &path_id) {
                    Some(orchestration::badgey::events::ThreadEvent::PathCached {
                        id,
                        nodes,
                        edges,
                        summary,
                    }) => Ok(json!({
                        "badgey_id": instance.id,
                        "path_id": id,
                        "status": "found",
                        "nodes": nodes,
                        "edges": edges,
                        "summary": summary,
                    })),
                    _ => Ok(json!({
                        "badgey_id": id,
                        "path_id": path_id,
                        "status": "not_found",
                    })),
                };
            }
            Some(WrapperCommand::BudgetExtend) => {
                let instance = self
                    .state
                    .badgey_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                self.badgey_action_result_note(
                    &instance,
                    &uuid::Uuid::new_v4().to_string(),
                    "budget_extended",
                    json!({"added_tokens": 50_000}),
                )?;
                return Ok(json!({
                    "badgey_id": id,
                    "status": "accepted",
                    "budget": self.badgey_observability(&instance)["budget"].clone(),
                }));
            }
            Some(WrapperCommand::RevertBrofileTo(version)) => {
                let instance = self
                    .state
                    .badgey_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                let proposal = self
                    .state
                    .badgey_proposals
                    .create(
                        &id,
                        orchestration::badgey::types::ProposalKind::Brofile,
                        json!({
                            "action": "revert_brofile",
                            "name": "badgey-persona",
                            "version": version,
                            "source": format!("artifact:brofile:badgey-persona@{version}"),
                        }),
                        Some(format!("revert-brofile:{version}")),
                    )
                    .map_err(|e| format!("creating brofile revert proposal: {e}"))?;
                self.badgey_write_event(
                    &instance,
                    orchestration::badgey::events::ThreadEvent::ProposalEmitted {
                        proposal_id: proposal.id.clone(),
                        kind: proposal.kind,
                        draft_ref: format!("badgey-persona@{version}"),
                        state: proposal.state,
                    },
                    None,
                )?;
                return Ok(json!({
                    "badgey_id": id,
                    "version": version,
                    "status": "proposal_created",
                    "proposal_id": proposal.id,
                }));
            }
            Some(WrapperCommand::TrustSubBro(label)) => {
                let instance = self
                    .state
                    .badgey_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                self.badgey_action_result_note(
                    &instance,
                    &uuid::Uuid::new_v4().to_string(),
                    "subbro_trusted",
                    json!({"label": label}),
                )?;
                return Ok(json!({
                    "badgey_id": id,
                    "sub_bro": label,
                    "status": "recorded",
                }));
            }
            None => {}
        }
        let instance = self
            .state
            .badgey_registry
            .get(&id)
            .map_err(|e| e.to_string())?;
        if !instance.provider.supports_resume() {
            return Err(format!("{} does not support resume", instance.provider));
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.state
            .badgey_registry
            .enqueue_resume(
                &id,
                orchestration::badgey::queue::PendingTurn {
                    turn_id: turn_id.clone(),
                    prompt: prompt.to_string(),
                },
            )
            .map_err(|e| e.to_string())?;
        let _permit = self
            .state
            .badgey_registry
            .wait_for_resume_turn(&id, &turn_id)
            .await
            .map_err(|e| e.to_string())?;

        let task_id = uuid::Uuid::new_v4().to_string();
        let turn_start = util::now_iso();
        let cwd = instance
            .provider
            .resolve_session_cwd(&instance.provider_session_id)
            .map(|p| p.to_string_lossy().into_owned())
            .or_else(|| Some(instance.scope.project_id.clone()));
        let (provider, _lens, exec_opts, env_overrides, _resolved_cwd, brofile_filters) =
            self.resolve_exec_target(Some("badgey-persona"), None, cwd.as_deref())?;
        let scope_bind =
            self.badgey_scope_bind(&id, &instance.thread_of_record_id, &instance.scope);
        let wrapped_user_prompt = format!("{scope_bind}\n{prompt}");
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(instance.provider_session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some("badgey-persona".to_string()),
            thread_id: Some(instance.thread_of_record_id.clone()),
            work_item_id: Some(id.as_str().to_string()),
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some("badgey-persona"),
                Some(instance.provider_session_id.as_str()),
                Some(instance.thread_of_record_id.as_str()),
                Some(id.as_str()),
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
        };
        let final_prompt = orch::apply_ambient(&wrapped_user_prompt, &ambient_ctx);
        let mut args = provider.build_resume_args(
            &instance.provider_session_id,
            &final_prompt,
            exec_opts.as_ref(),
        );
        let dispatch_filters = match resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            &task_id,
            brofile_filters.as_ref(),
            None,
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Err(e),
        };
        let effective_filters = dispatch_filters.filters.clone();
        args.extend(dispatch_filters.args);
        let task = orch::spawn_task(
            task_id.clone(),
            provider,
            args,
            instance.provider_session_id.clone(),
            cwd.clone(),
            env_overrides,
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some("badgey".to_string()),
            Some("agent:badgey@v1".to_string()),
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        let completed = orch::wait_for_task_with_timeout(&task, timeout_seconds).await;
        let result = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        let action_results = self
            .badgey_post_process_turn(&instance, &turn_start)
            .await?;
        let refs_consumed = self.badgey_refs_consumed_from_result(&result);
        self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::Turn {
                turn_id: self.badgey_next_turn_id(&instance.thread_of_record_id),
                mode: "answer".to_string(),
                caller: orchestration::badgey::events::CallerRef {
                    provider,
                    session_id: instance.provider_session_id.clone(),
                },
                question: prompt.to_string(),
                bundle_summary: result
                    .get("status")
                    .and_then(Value::as_str)
                    .unwrap_or("completed")
                    .to_string(),
                refs_consumed,
                proposals_emitted: action_results
                    .iter()
                    .filter_map(|value| {
                        value
                            .get("proposal_id")
                            .and_then(Value::as_str)
                            .map(String::from)
                    })
                    .collect(),
            },
            Some(task_id.clone()),
        )?;
        Ok(json!({
            "badgey_id": id,
            "task_id": task_id,
            "session_id": instance.provider_session_id,
            "provider": provider,
            "thread_id": instance.thread_of_record_id,
            "result": result,
            "actions": action_results,
            "merged_filters": effective_filters,
        }))
    }

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
        let (provider, lens, exec_opts, env_overrides, cwd, brofile_filters) =
            self.resolve_exec_target(Some(brofile), None, Some(project_dir))?;
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
        });
        Ok(json!({
            "badgey_id": id,
            "status": "dismissed",
            "thread_id": instance.thread_of_record_id,
        }))
    }

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

    #[tool(
        name = "badgey_exec",
        description = "Start a Badgey consultant instance for a project scope and return its badgey_id, provider session, task, and thread-of-record ids."
    )]
    pub(crate) async fn badgey_exec(
        &self,
        Parameters(p): Parameters<BadgeyExecParams>,
    ) -> CallToolResult {
        match self
            .badgey_exec_internal(p.project_dir, p.brief, Some("agent:badgey@v1".to_string()))
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_resume",
        description = "Send a turn to an existing Badgey instance. Mechanical commands such as `dismiss` are handled by the wrapper before provider resume."
    )]
    pub(crate) async fn badgey_resume(
        &self,
        Parameters(p): Parameters<BadgeyResumeParams>,
    ) -> CallToolResult {
        match self
            .badgey_resume_internal(&p.badgey_id, &p.prompt, p.timeout_seconds)
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_ask",
        description = "Question-shaped alias for badgey_resume."
    )]
    pub(crate) async fn badgey_ask(
        &self,
        Parameters(p): Parameters<BadgeyAskParams>,
    ) -> CallToolResult {
        match self
            .badgey_resume_internal(&p.badgey_id, &p.question, p.timeout_seconds)
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_dismiss",
        description = "Dismiss a Badgey instance, drain queued turns, write a dismiss event, and resolve its thread of record."
    )]
    pub(crate) fn badgey_dismiss(
        &self,
        Parameters(p): Parameters<BadgeyDismissParams>,
    ) -> CallToolResult {
        match self.badgey_dismiss_internal(&p.badgey_id, p.reason) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_status",
        description = "Inspect one Badgey instance, including queue status and proposals; without badgey_id, returns active instances."
    )]
    pub(crate) fn badgey_status(
        &self,
        Parameters(p): Parameters<BadgeyStatusParams>,
    ) -> CallToolResult {
        match self.badgey_status_internal(p.badgey_id.as_deref()) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_list",
        description = "List Badgey instances and their thread/session bindings."
    )]
    pub(crate) fn badgey_list(
        &self,
        Parameters(p): Parameters<BadgeyListParams>,
    ) -> CallToolResult {
        match self.badgey_list_internal(p.include_dismissed.unwrap_or(false)) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_scout",
        description = "Ask Badgey to author scout sub-charters for a focused question; wrapper post-processing dispatches emitted scout actions."
    )]
    pub(crate) async fn badgey_scout(
        &self,
        Parameters(p): Parameters<BadgeyScoutParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(id) => id,
            Err(err) => return Self::err_text(&err),
        };
        let instance = match self.state.badgey_registry.get(&id) {
            Ok(instance) => instance,
            Err(err) => return Self::err_text(&err.to_string()),
        };
        let scout_id = format!("scout-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
        if let Err(err) = self.badgey_write_event(
            &instance,
            orchestration::badgey::events::ThreadEvent::ScoutDispatched {
                scout_id: scout_id.clone(),
                scout_thread_id: instance.thread_of_record_id.clone(),
                charters: vec![p.charter.clone()],
            },
            None,
        ) {
            return Self::err_text(&err);
        }
        let prompt = format!(
            "Scout mode. Use scout_id={scout_id}. Author wrapper-mediated sub-bro charters for this question and emit bg-action-spawn-subbro notes with this scout_id as needed.\n\nCharter: {}",
            p.charter
        );
        match self
            .badgey_resume_internal(&p.badgey_id, &prompt, p.timeout_seconds)
            .await
        {
            Ok(mut value) => {
                value["scout_id"] = Value::String(scout_id);
                value["scout_thread_id"] = Value::String(instance.thread_of_record_id);
                Self::ok_json(&value)
            }
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_collect",
        description = "Collect scout/sub-bro events for a Badgey instance or scout id."
    )]
    pub(crate) fn badgey_collect(
        &self,
        Parameters(p): Parameters<BadgeyCollectParams>,
    ) -> CallToolResult {
        match self.badgey_collect_internal(p.scout_id.as_deref(), p.badgey_id.as_deref()) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_triage_inbox",
        description = "Produce a Badgey-shaped inbox triage proposal sheet for stale/open work in a scope."
    )]
    pub(crate) fn badgey_triage_inbox(
        &self,
        Parameters(p): Parameters<BadgeyTriageInboxParams>,
    ) -> CallToolResult {
        match self.badgey_triage_inbox_internal(p.scope, p.since, p.badgey_id) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_close_loops",
        description = "Classify dispatched tasks without done notes; never synthesizes executor done notes."
    )]
    pub(crate) fn badgey_close_loops(
        &self,
        Parameters(p): Parameters<BadgeyCloseLoopsParams>,
    ) -> CallToolResult {
        match self.badgey_close_loops_internal(p.window_days, p.project_dir) {
            Ok(value) => Self::ok_json(&value),
            Err(err) => Self::err_text(&err),
        }
    }

    #[tool(
        name = "badgey_proposals_list",
        description = "List BadgeyProposal records owned by an instance. Returns full proposal objects (id, kind, state, draft, created_at, updated_at, events, applied_task_id) sorted by proposal_id number. Optional `since` filter (ISO timestamp) restricts to proposals created at or after that moment — useful for reading proposals emitted by the most recent Badgey turn. Used by the per-channel triage workflow's ForeachPostProposal node to iterate proposals freshly emitted by the synthesis turn."
    )]
    pub(crate) fn badgey_proposals_list(
        &self,
        Parameters(p): Parameters<BadgeyProposalsListParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => return Self::err_text(&e),
        };
        let proposals = match self.state.badgey_proposals.list_by_instance(&id) {
            Ok(v) => v,
            Err(e) => return Self::err_text(&format!("listing proposals: {e}")),
        };
        let filtered: Vec<_> = proposals
            .into_iter()
            .filter(|proposal| {
                p.since
                    .as_deref()
                    .is_none_or(|since| proposal.created_at.as_str() >= since)
            })
            .filter(|proposal| p.only_pending != Some(true) || !proposal.is_terminal())
            .collect();
        Self::ok_json(&json!({
            "badgey_id": p.badgey_id,
            "since": p.since,
            "count": filtered.len(),
            "proposals": filtered,
        }))
    }

    #[tool(
        name = "badgey_ensure_for_channel",
        description = "Get-or-create the system Badgey instance that authors triage briefs for a Slack-bound project. Reads the (team_id, channel_id) binding to resolve the project scope, looks up the binding's badgey_id; if absent or the instance has been dismissed, exec a fresh Badgey instance, persist its id back on the binding, and return it. Used by the per-channel triage workflow's EnsureInstance node."
    )]
    pub(crate) async fn badgey_ensure_for_channel(
        &self,
        Parameters(p): Parameters<EnsureBadgeyForChannelParams>,
    ) -> CallToolResult {
        if p.team_id.trim().is_empty() {
            return Self::err_text("team_id is required");
        }
        if p.channel_id.trim().is_empty() {
            return Self::err_text("channel_id is required");
        }
        let binding = match self
            .state
            .slack_channel_bindings
            .lookup(&p.team_id, &p.channel_id)
        {
            Some(b) => b,
            None => {
                return Self::err_text(&format!(
                    "no binding for team={} channel={} — run bro_slack_bind first",
                    p.team_id, p.channel_id
                ));
            }
        };
        let scope = p
            .scope_override
            .clone()
            .unwrap_or_else(|| binding.project_dir.clone());

        // Resume existing instance when present + still active.
        if let Some(ref bid) = binding.badgey_id {
            if let Ok(parsed) = bid.parse::<orchestration::badgey::types::BadgeyId>() {
                match self.state.badgey_registry.get(&parsed) {
                    Ok(instance) => {
                        return Self::ok_json(&json!({
                            "badgey_id": bid,
                            "thread_id": instance.thread_of_record_id,
                            "project_id": instance.scope.project_id,
                            "session_id": instance.provider_session_id,
                            "created": false,
                        }));
                    }
                    Err(e) => {
                        tracing::info!(
                            badgey_id = %bid,
                            "ensure_badgey_for_channel: stored badgey unusable ({e}) — creating fresh"
                        );
                    }
                }
            }
        }

        // Create a new instance and persist its id back on the binding.
        let initial_brief = format!(
            "Slack daily-brief triage agent for #{} (project: {}). \
             Operate in triage + corpus-mining mode: classify stale work-items, \
             score graph-edge meatiness, dispatch focused scouts when warranted, \
             and synthesize structured proposals for review.",
            binding.channel_name.as_deref().unwrap_or(&p.channel_id),
            scope,
        );
        let exec_result = match self
            .badgey_exec_internal(
                Some(scope.clone()),
                Some(initial_brief),
                Some(format!("badgey-slack-{}", p.channel_id)),
            )
            .await
        {
            Ok(v) => v,
            Err(e) => return Self::err_text(&format!("badgey_exec failed: {e}")),
        };
        let new_badgey_id = match exec_result.get("badgey_id").and_then(|v| v.as_str()) {
            Some(id) => id.to_string(),
            None => return Self::err_text("badgey_exec didn't return a badgey_id"),
        };
        if let Err(e) = self.state.slack_channel_bindings.set_badgey_id(
            &p.team_id,
            &p.channel_id,
            Some(new_badgey_id.clone()),
        ) {
            tracing::warn!(
                badgey_id = %new_badgey_id,
                "ensure_badgey_for_channel: persisting badgey_id on binding failed: {e}"
            );
        }
        Self::ok_json(&json!({
            "badgey_id": new_badgey_id,
            "thread_id": exec_result.get("thread_id"),
            "project_id": exec_result.get("project_id"),
            "session_id": exec_result.get("session_id"),
            "task_id": exec_result.get("task_id"),
            "created": true,
        }))
    }

    #[tool(
        name = "badgey_apply_proposal",
        description = "Apply a stored BadgeyProposal — drives the wrapper's full apply path: state-machine transition (Pending/Failed → Applying), kind-specific dispatch (artifact_promotion → bbox_artifact_install; redispatch_task → spawn_privileged_task with the proposal's prompt; workflow_install/agent_install/packet_install → matching artifact install), record applied_task_id, transition (Applying → Applied | Failed). Returns the apply result with status. One-shot wrapper — for the Slack-reaction flow prefer the split `badgey_proposal_begin_apply` + `badgey_proposal_complete_apply` pair so the workflow engine tracks the dispatched bro natively as an actor node."
    )]
    pub(crate) async fn badgey_apply_proposal(
        &self,
        Parameters(p): Parameters<BadgeyApplyProposalParams>,
    ) -> CallToolResult {
        // Always return Ok with explicit `status` + `summary` fields.
        //
        // status is one of:
        //   "applied"         — fresh apply succeeded
        //   "already_applied" — proposal was already in Applied state
        //   "failed"          — apply path raised
        //   "bad_input"       — badgey_id couldn't parse
        //
        // summary is a one-line human-readable description that the
        // Slack-emit summary template can interpolate without
        // worrying about which fields are present per kind/outcome:
        //   applied (RedispatchTask):  "dispatched task `<task_id>`"
        //   applied (artifact_*):      "installed `<artifact_ref>`"
        //   already_applied:           "already applied (prior task `<id>`)"
        //   failed / bad_input:        "<error>"
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "bad_input",
                    "error": e.clone(),
                    "summary": e,
                    "badgey_id": p.badgey_id,
                }));
            }
        };
        let result = self
            .badgey_apply_proposal_internal(&id, &p.proposal_id, p.retry_failed.unwrap_or(false))
            .await;
        match result {
            Ok(mut value) => {
                let already = value
                    .get("already_applied")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let status = if already {
                    "already_applied"
                } else {
                    "applied"
                };
                let summary = if already {
                    let prior = value
                        .get("prior_task_id")
                        .and_then(Value::as_str)
                        .unwrap_or("");
                    if prior.is_empty() {
                        "already applied".to_string()
                    } else {
                        format!("already applied (prior task `{prior}`)")
                    }
                } else if let Some(task_id) = value.get("task_id").and_then(Value::as_str) {
                    format!("dispatched task `{task_id}`")
                } else if let Some(artifact_ref) = value.get("artifact_ref").and_then(Value::as_str)
                {
                    format!("installed `{artifact_ref}`")
                } else {
                    "applied".to_string()
                };
                if let Some(obj) = value.as_object_mut() {
                    obj.entry("status".to_string())
                        .or_insert_with(|| Value::String(status.into()));
                    obj.insert("summary".into(), Value::String(summary));
                }
                Self::ok_json(&value)
            }
            Err(e) => Self::ok_json(&json!({
                "status": "failed",
                "error": e.clone(),
                "summary": e,
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "badgey_proposal_begin_apply",
        description = "Phase 1 of the split apply path. Transitions a proposal Pending|Failed → Applying and returns dispatch parameters (prompt + brofile + label for redispatch_task; artifact_kind + source + version for artifact installs). Does NOT spawn the bro or install the artifact — the workflow caller does that via an actor node or `bbox_artifact_install` mcp_call, then calls `badgey_proposal_complete_apply` with the outcome. Lets the engine track the dispatched work natively (actor task lifecycle, retries, gates) instead of opaquely spawning behind a wrapper."
    )]
    pub(crate) async fn badgey_proposal_begin_apply(
        &self,
        Parameters(p): Parameters<BadgeyProposalBeginApplyParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "outcome": "rejected",
                    "reason": "bad_input",
                    "error": e.clone(),
                    "badgey_id": p.badgey_id,
                }));
            }
        };
        match self
            .badgey_proposal_begin_apply_internal(
                &id,
                &p.proposal_id,
                p.retry_failed.unwrap_or(false),
            )
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::ok_json(&json!({
                "outcome": "rejected",
                "reason": "internal_error",
                "error": e.clone(),
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }

    #[tool(
        name = "badgey_proposal_complete_apply",
        description = "Phase 2 of the split apply path. Given the outcome of the dispatched work (passed in `outcome`: `completed` / `failed` / `cancelled` / `timed_out`), transitions the proposal Applying → Applied or Applying → Failed and writes the audit decision. Always returns `{status: applied|failed, ...}` so the workflow's PostOutcome node can read the final state and pick the badge."
    )]
    pub(crate) async fn badgey_proposal_complete_apply(
        &self,
        Parameters(p): Parameters<BadgeyProposalCompleteApplyParams>,
    ) -> CallToolResult {
        let id = match self.badgey_parse_id(&p.badgey_id) {
            Ok(parsed) => parsed,
            Err(e) => {
                return Self::ok_json(&json!({
                    "status": "failed",
                    "error": e.clone(),
                    "badgey_id": p.badgey_id,
                }));
            }
        };
        match self
            .badgey_proposal_complete_apply_internal(
                &id,
                &p.proposal_id,
                &p.outcome,
                p.task_id.as_deref(),
                p.artifact_ref.as_deref(),
                p.summary.as_deref(),
            )
            .await
        {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::ok_json(&json!({
                "status": "failed",
                "error": e.clone(),
                "badgey_id": p.badgey_id,
                "proposal_id": p.proposal_id,
            })),
        }
    }
}

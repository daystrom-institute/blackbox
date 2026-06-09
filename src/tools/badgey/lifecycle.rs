use std::sync::Arc;

use crate::notes;
use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::providers::Provider;
use crate::orchestration::providers::dispatch_prelude::*;
use crate::server::progress::{cleanup_policy_file_when_done, resolve_dispatch_filters};
use crate::server::state::BlackboxServer;
use crate::threads;
use crate::util;
use serde_json::{Value, json};

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
        let (
            provider,
            lens,
            exec_opts,
            env_overrides,
            cwd,
            brofile_filters,
            _coerce_workspace,
            brofile_context,
        ) = self.resolve_exec_target(Some("badgey-persona"), None, Some(&scope.project_id))?;
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            brofile_context.as_ref(),
        );

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
            coerce_workspace: false,
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
        let dispatch_filters =
            resolve_dispatch_filters(provider, cwd.as_deref(), false, &task_id, Some(&filters))?;
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
            Some(self.state.roster_events()),
            bro_label.clone(),
            bro_label,
            Some(self.state.system_events.clone()),
            // badgey_launch_exec is the badgey runtime's first-turn
            // launch — operator-initiated persona. See the Slice 1b
            // dispatch note: badgey is classed as AgentDispatch
            // (user-driven tool that dispatches an agent context).
            bro_core::Origin::AgentDispatch,
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
                origin: None,
            })
            .map_err(|e| format!("opening badgey thread of record: {e:#}"))?;
        // This sync thread helper cannot await; threads persistence is write-behind here.
        self.state.threads_persister.request();
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
        let continue_result = self.state.threads.write().thread(&threads::ThreadParams {
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
            origin: None,
        });
        if continue_result.is_ok() {
            // This sync thread helper cannot await; threads persistence is write-behind here.
            self.state.threads_persister.request();
        }
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
        let (
            provider,
            _lens,
            exec_opts,
            mut env_overrides,
            _resolved_cwd,
            brofile_filters,
            _coerce_workspace,
            brofile_context,
        ) = self.resolve_exec_target(Some("badgey-persona"), None, cwd.as_deref())?;
        // Resume must honor the policy the original badgey dispatch
        // was launched under, not whatever badgey-persona says today.
        // Look up the lease for this provider-session and prefer its
        // captured brofile_context for both enforcement and env.
        let resume_lease = crate::orchestration::allocator::lookup_lease_for_session_any_provider(
            &self.state.store_dir,
            &self.state.task_store.read(),
            &instance.provider_session_id,
        );
        let effective_provider = resume_lease
            .as_ref()
            .map(|lease| lease.provider)
            .unwrap_or(instance.provider);
        let effective_context = resume_lease
            .as_ref()
            .and_then(|lease| lease.brofile_context.as_ref())
            .or(brofile_context.as_ref());
        crate::orchestration::brofile::enforce_provider_defaults(
            effective_provider,
            effective_context,
        )?;
        if let Some(lease) = resume_lease.as_ref() {
            env_overrides = crate::orchestration::brofile::resolve_provider_env(
                effective_provider,
                lease.account.as_deref(),
                lease.model.as_deref(),
                &self.state.store_dir,
                effective_context,
            );
        }
        let exec_opts = resume_lease
            .as_ref()
            .and_then(|lease| {
                crate::orchestration::allocator::exec_opts_for_lane(
                    &crate::orchestration::allocator::RuntimeLane {
                        provider: lease.provider,
                        account: lease.account.clone(),
                        tier: lease.tier.clone(),
                        model: lease.model.clone(),
                        effort: lease.effort.clone(),
                        capabilities: lease.capabilities.clone(),
                    },
                )
            })
            .or(exec_opts);
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
            provider: Some(effective_provider),
            coerce_workspace: false,
        };
        let final_prompt = orch::apply_ambient(&wrapped_user_prompt, &ambient_ctx);
        let mut args = effective_provider.build_resume_args(
            &instance.provider_session_id,
            &final_prompt,
            orchestration::providers::exec_opts_with_provider_defaults(
                exec_opts,
                effective_context,
            )
            .as_ref(),
        );
        let dispatch_filters = match resolve_dispatch_filters(
            effective_provider,
            cwd.as_deref(),
            false,
            &task_id,
            brofile_filters.as_ref(),
        ) {
            Ok(df) => df,
            Err(e) => return Err(e),
        };
        let effective_filters = dispatch_filters.filters.clone();
        args.extend(dispatch_filters.args);
        let task = orch::spawn_task(
            task_id.clone(),
            effective_provider,
            args,
            instance.provider_session_id.clone(),
            cwd.clone(),
            env_overrides,
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(self.state.roster_events()),
            Some("badgey".to_string()),
            Some("agent:badgey@v1".to_string()),
            Some(self.state.system_events.clone()),
            // badgey_resume_internal is the badgey runtime's
            // continuation dispatch; same source class as
            // badgey_launch_exec (AgentDispatch).
            bro_core::Origin::AgentDispatch,
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
                    provider: effective_provider,
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
}

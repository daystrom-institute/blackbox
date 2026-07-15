use std::sync::Arc;

use crate::notes;
use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::consultant::descriptor::ConsumerDescriptor;
use crate::orchestration::providers::Provider;
use crate::orchestration::providers::dispatch_prelude::*;
use crate::server::progress::{cleanup_policy_file_when_done, resolve_dispatch_filters};
use crate::server::state::BlackboxServer;
use crate::threads;
use crate::util;
use serde_json::{Value, json};

impl BlackboxServer {
    pub(crate) fn consultant_thread_id_from_open_result(
        &self,
        result: &str,
    ) -> Result<String, String> {
        let re = regex::Regex::new(r"Thread created: (thread-[0-9a-f]{8})")
            .map_err(|e| format!("internal regex error: {e}"))?;
        re.captures(result)
            .and_then(|caps| caps.get(1).map(|m| m.as_str().to_string()))
            .ok_or_else(|| format!("could not parse thread id from bbox_thread result: {result}"))
    }

    pub(crate) fn consultant_scope_bind(
        &self,
        descriptor: &'static ConsumerDescriptor,
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
            .consultant_proposals
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
            .consultant_registry
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
        let budget_remaining =
            descriptor.turn_budget_tokens + budget_extensions * descriptor.turn_budget_tokens;
        format!(
            "[{name}-scope]\n{name}_id: {id}\nthread_of_record: {thread_id}\nproject: {project}\ncurrent_time: {current_time}\nbrief: {brief}\nqueue: {queue_status}\nrecent_paths: {recent_paths}\nrecent_proposals: {recent_proposals}\nbudget_remaining: {budget_remaining}\n[/{name}-scope]\n",
            name = descriptor.name,
            current_time = util::now_iso(),
            project = scope.project_id
        )
    }

    pub(crate) fn consultant_write_event(
        &self,
        descriptor: &'static ConsumerDescriptor,
        instance: &orchestration::badgey::registry::BadgeyInstance,
        event: orchestration::badgey::events::ThreadEvent,
        task_id: Option<String>,
    ) -> Result<String, String> {
        let kind = event.note_kind().to_string();
        let body = serde_json::to_string(&event)
            .map_err(|e| format!("serializing consultant thread event: {e}"))?;
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
                bro: Some(descriptor.name.to_string()),
            })
            .map_err(|e| format!("writing consultant thread event: {e:#}"))
    }

    pub(crate) fn consultant_launch_exec(
        &self,
        descriptor: &'static ConsumerDescriptor,
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
        ) =
            self.resolve_exec_target(Some(descriptor.brofile_ref), None, Some(&scope.project_id))?;
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            brofile_context.as_ref(),
        );

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = "pending".to_string();
        let scope_bind = self.consultant_scope_bind(descriptor, id, thread_id, scope);
        let prompt = format!("{}\n{}\n", scope_bind, descriptor.exec_init_prompt);
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(descriptor.brofile_ref.to_string()),
            thread_id: Some(thread_id.to_string()),
            work_item_id: Some(id.as_str().to_string()),
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(descriptor.brofile_ref),
                Some(session_id.as_str()),
                Some(thread_id),
                Some(id.as_str()),
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
            coerce_workspace: false,
        };
        let dispatch_context = ambient_ctx.dispatch_context(lens.as_deref());
        let mut args = provider.build_exec_args(
            &prompt,
            Some(&dispatch_context),
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
            orch::capabilities::harness_session_services(&self.state),
            Some(self.state.system_events.clone()),
            // consultant_launch_exec is the consultant runtime's first-turn
            // launch — operator-initiated persona. See the Slice 1b
            // dispatch note: consultant exec is classed as AgentDispatch
            // (user-driven tool that dispatches an agent context).
            bro_core::Origin::AgentDispatch,
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        Ok((task, provider, session_id, effective_filters))
    }

    pub(crate) async fn consultant_wait_for_observed_session_id(
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
                "provider session id was not observed before consultant registration timeout"
                    .to_string()
            })?
    }

    pub(crate) async fn consultant_exec_internal(
        &self,
        descriptor: &'static ConsumerDescriptor,
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
        let id = descriptor.generate_id();
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
                name: Some(format!("{}:{}", descriptor.name, id.as_str())),
                id: None,
                topic: Some(format!(
                    "{} consultation: {}",
                    descriptor.display_name,
                    brief.as_deref().unwrap_or("general consultation")
                )),
                project: Some(project_id.clone()),
                session_id: None,
                provider: None,
                session_name: None,
                handoff_doc: None,
                note: Some(format!("{} thread of record", descriptor.display_name)),
                target: None,
                target_type: None,
                edge: None,
                promoted_to: None,
                kind: Some("work_item".to_string()),
                origin: None,
            })
            .map_err(|e| format!("opening consultant thread of record: {e:#}"))?;
        // This sync thread helper cannot await; threads persistence is write-behind here.
        self.state.threads_persister.request();
        let thread_id = self.consultant_thread_id_from_open_result(&thread_result)?;
        let (task, provider, _initial_session_id, merged_filters) =
            self.consultant_launch_exec(descriptor, &id, &scope, &thread_id, bro_label)?;
        let task_id = task.inner.lock().id.clone();
        let session_id = match self
            .consultant_wait_for_observed_session_id(&task, 10.0)
            .await
        {
            Ok(session_id) => session_id,
            Err(err) => {
                let _ = self.state.notes.write().create(&notes::NoteParams {
                    kind: "surprise".to_string(),
                    body: json!({
                        "event": format!("{}_exec_unobserved_session", descriptor.name),
                        "consultant_id": id,
                        "task_id": task_id,
                        "reason": err,
                    })
                    .to_string(),
                    task_id: Some(task_id),
                    session_id: None,
                    project: Some(project_id),
                    thread_id: Some(thread_id),
                    provider: Some(provider.as_str().to_string()),
                    bro: Some(descriptor.name.to_string()),
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
            .consultant_registry
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
            session_name: Some(descriptor.name.to_string()),
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
        self.consultant_write_event(
            descriptor,
            &instance,
            orchestration::badgey::events::ThreadEvent::Exec {
                brofile_version: descriptor.brofile_ref.to_string(),
                scope,
                charter: brief.unwrap_or_else(|| "general consultation".to_string()),
                provider,
                provider_session_id: session_id.clone(),
            },
            Some(task_id.clone()),
        )?;
        let mut out = json!({
            "consultant_id": id,
            "task_id": task_id,
            "session_id": session_id,
            "provider": provider,
            "thread_id": thread_id,
            "status": "running",
            "resolved_brofile": descriptor.brofile_ref,
            "merged_filters": merged_filters,
        });
        // Legacy consumer-keyed id (e.g. `badgey_id`) kept for wire compat.
        out[format!("{}_id", descriptor.name)] = json!(id);
        Ok(out)
    }

    pub(crate) async fn consultant_resume_internal(
        &self,
        descriptor: &'static ConsumerDescriptor,
        raw_id: &str,
        prompt: &str,
        timeout_seconds: Option<f64>,
    ) -> Result<Value, String> {
        use orchestration::badgey::commands::{WrapperCommand, parse_command};

        let id = descriptor
            .parse_id(raw_id)
            .map_err(|e| format!("error.bad_input(code=invalid_{}_id): {e}", descriptor.name))?;
        // Consumer hook dispatch: the wrapper-command grammar and its
        // handlers are code-owned hook sets the descriptor selects.
        let command = match descriptor.hooks {
            crate::orchestration::consultant::descriptor::ConsumerHooks::Badgey => {
                parse_command(prompt)
            }
            crate::orchestration::consultant::descriptor::ConsumerHooks::None => None,
        };
        match command {
            Some(WrapperCommand::Dismiss) => {
                return self.badgey_dismiss_internal(raw_id, Some("wrapper command".to_string()));
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
                    .consultant_registry
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
                    .consultant_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                self.badgey_action_result_note(
                    &instance,
                    &uuid::Uuid::new_v4().to_string(),
                    "budget_extended",
                    json!({"added_tokens": descriptor.turn_budget_tokens}),
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
                    .consultant_registry
                    .get(&id)
                    .map_err(|e| e.to_string())?;
                let proposal = self
                    .state
                    .consultant_proposals
                    .create(
                        &id,
                        orchestration::badgey::types::ProposalKind::Brofile.as_str(),
                        json!({
                            "action": "revert_brofile",
                            "name": descriptor.brofile_ref,
                            "version": version,
                            "source": format!(
                                "artifact:brofile:{}@{version}",
                                descriptor.brofile_ref
                            ),
                        }),
                        Some(format!("revert-brofile:{version}")),
                    )
                    .map_err(|e| format!("creating brofile revert proposal: {e}"))?;
                self.consultant_write_event(
                    descriptor,
                    &instance,
                    orchestration::badgey::events::ThreadEvent::ProposalEmitted {
                        proposal_id: proposal.id.clone(),
                        kind: orchestration::badgey::types::ProposalKind::Brofile
                            .as_str()
                            .to_string(),
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
                    .consultant_registry
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
            .consultant_registry
            .get(&id)
            .map_err(|e| e.to_string())?;
        if !instance.provider.supports_resume() {
            return Err(format!("{} does not support resume", instance.provider));
        }
        let turn_id = uuid::Uuid::new_v4().to_string();
        self.state
            .consultant_registry
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
            .consultant_registry
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
            lens,
            exec_opts,
            mut env_overrides,
            _resolved_cwd,
            brofile_filters,
            _coerce_workspace,
            brofile_context,
        ) = self.resolve_exec_target(Some(descriptor.brofile_ref), None, cwd.as_deref())?;
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
        let scope_bind = self.consultant_scope_bind(
            descriptor,
            &id,
            &instance.thread_of_record_id,
            &instance.scope,
        );
        let wrapped_user_prompt = format!("{scope_bind}\n{prompt}");
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(instance.provider_session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(descriptor.brofile_ref.to_string()),
            thread_id: Some(instance.thread_of_record_id.clone()),
            work_item_id: Some(id.as_str().to_string()),
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(descriptor.brofile_ref),
                Some(instance.provider_session_id.as_str()),
                Some(instance.thread_of_record_id.as_str()),
                Some(id.as_str()),
            ),
            completion_contract: Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(effective_provider),
            coerce_workspace: false,
        };
        // Full dispatch context on resume, persona included
        // (dispatch-prompt-slots.md §6).
        let dispatch_context = ambient_ctx.dispatch_context(lens.as_deref());
        let mut args = effective_provider.build_resume_args(
            &instance.provider_session_id,
            &wrapped_user_prompt,
            Some(&dispatch_context),
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
            Some(descriptor.name.to_string()),
            Some(descriptor.agent_ref.to_string()),
            orch::capabilities::harness_session_services(&self.state),
            Some(self.state.system_events.clone()),
            // consultant_resume_internal is the consultant runtime's
            // continuation dispatch; same source class as
            // consultant_launch_exec (AgentDispatch).
            bro_core::Origin::AgentDispatch,
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        let completed = orch::wait_for_task_with_timeout(&task, timeout_seconds).await;
        let result = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        // Consumer intent post-processing: parses the consumer's intent-note
        // grammar and dispatches the code-owned hook set the descriptor
        // selects.
        let action_results = match descriptor.hooks {
            crate::orchestration::consultant::descriptor::ConsumerHooks::Badgey => {
                self.badgey_post_process_turn(&instance, &turn_start)
                    .await?
            }
            crate::orchestration::consultant::descriptor::ConsumerHooks::None => Vec::new(),
        };
        let refs_consumed = self.badgey_refs_consumed_from_result(&result);
        self.consultant_write_event(
            descriptor,
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
        let mut out = json!({
            "consultant_id": id,
            "task_id": task_id,
            "session_id": instance.provider_session_id,
            "provider": provider,
            "thread_id": instance.thread_of_record_id,
            "result": result,
            "actions": action_results,
            "merged_filters": effective_filters,
        });
        // Legacy consumer-keyed id (e.g. `badgey_id`) kept for wire compat.
        out[format!("{}_id", descriptor.name)] = json!(id);
        Ok(out)
    }
}

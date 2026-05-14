use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::dispatch_tools()
}

#[tool_router(router = dispatch_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_exec",
        description = "Launch a fresh agent task/session and return {taskId, sessionId} immediately; do not use for continuity."
    )]
    pub(crate) async fn bro_exec(&self, Parameters(p): Parameters<ExecParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let store_dir = self.state.store_dir.clone();

        let (
            provider,
            lens,
            exec_opts,
            env_overrides,
            cwd,
            brofile_filters,
            brofile_coerce_workspace,
        ) = match self.resolve_exec_target(
            p.bro.as_deref(),
            p.provider.as_deref(),
            p.project_dir.as_deref(),
        ) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&e),
        };

        // Pre-generate task_id so it lands in the ambient [scope] block
        // before subprocess launch — the primary correlation key for
        // bbox_note emissions regardless of when the provider itself
        // emits a session ID.
        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if matches!(provider, Provider::Claude) {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let coerce_workspace = p.coerce_workspace.unwrap_or(brofile_coerce_workspace);
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                p.bro.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
            coerce_workspace,
        };
        let final_prompt = orch::apply_brofile_lens(
            &orch::apply_ambient(&p.prompt, &ambient_ctx),
            lens.as_deref(),
        );
        let mut args = provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = match resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
            p.surface.as_deref(),
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Self::err_text(&e),
        };
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            None,
            None,
            Some(self.state.system_events.clone()),
        );

        // Register Gemini policy-file cleanup once the task terminates.
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);

        // If targeting a named bro in a team, record the task
        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_resume",
        description = "Continue an existing session with a follow-up; single-flight per provider session and the continuity path after bro_exec."
    )]
    pub(crate) async fn bro_resume(
        &self,
        Parameters(p): Parameters<ResumeParams>,
    ) -> CallToolResult {
        let store_dir = self.state.store_dir.clone();

        let (
            provider,
            session_id,
            _lens,
            exec_opts,
            env_overrides,
            cwd,
            brofile_filters,
            brofile_coerce_workspace,
        ) = match self.resolve_resume_target(
            p.bro.as_deref(),
            p.session_id.as_deref(),
            p.provider.as_deref(),
            p.project_dir.as_deref(),
        ) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&e),
        };

        if !provider.supports_resume() {
            return Self::err_text(&format!("{provider} does not support resume"));
        }

        // Auto-resolve cwd from the session's own recorded origin so
        // agents can resurrect each other across repo boundaries without
        // the caller threading project_dir. Gemini gets a hard refuse on
        // miss because its CLI silently forks a fresh session when the
        // UUID isn't in the cwd's project hash folder (aliasing the
        // resumed session). Claude/Codex error loudly on miss — fall
        // through to the caller's cwd and let them surface the failure.
        let cwd = match provider.resolve_session_cwd(&session_id) {
            Some(p) => Some(p.to_string_lossy().into_owned()),
            None if provider == Provider::Gemini => {
                return Self::err_text(&format!(
                    "Gemini session {session_id} not found in ~/.gemini/tmp/*/chats. Refusing to resume because Gemini silently forks a new session when the UUID isn't in the cwd's project folder (aliasing the resumed session). Verify the session ID or re-dispatch.",
                ));
            }
            None => cwd,
        };

        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let task_id = uuid::Uuid::new_v4().to_string();
        let resume_lease = match try_acquire_resume_lease(
            &self.state.task_store,
            self.state.resume_leases.as_ref(),
            provider,
            &session_id,
        ) {
            Ok(lease) => lease,
            Err(err) => return Self::err_text(&err),
        };

        // Re-apply ambient on resume: each resume is its own dispatch with a
        // fresh task_id, and the per-turn recall directive + completion
        // contract need to ride with every follow-up (memory-file
        // reinforcement decays at depth). The brofile lens was injected on
        // exec and lives in the transcript — not re-prepended here.
        let coerce_workspace = p.coerce_workspace.unwrap_or(brofile_coerce_workspace);
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: p.bro.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                p.bro.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: if allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion,
            provider: Some(provider),
            coerce_workspace,
        };
        let wrapped_prompt = orch::apply_ambient(&p.prompt, &ambient_ctx);

        let mut args = provider.build_resume_args(&session_id, &wrapped_prompt, exec_opts.as_ref());
        // Filters (mechanical recursion guard + user-configured allow/
        // disallow) must ride with every dispatch — exec AND resume.
        // Without this, a resumed session re-acquires the orchestration
        // tool surface the recursion guard was meant to deny.
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());
        let extra = combine_dispatch_filters(brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = match resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            allow_recursion,
            &task_id,
            extra.as_ref(),
            p.surface.as_deref(),
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Self::err_text(&e),
        };
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            provider,
            args,
            session_id,
            cwd,
            env_overrides,
            store_dir,
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            None,
            None,
            Some(self.state.system_events.clone()),
        );
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        release_resume_lease_when_done(task.clone(), resume_lease);

        if let Some(bro_name) = &p.bro {
            self.record_task_to_bro(bro_name, &task);
        }

        let inner = task.inner.lock();
        Self::ok_json(&json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        }))
    }

    #[tool(
        name = "bro_wait",
        description = "Block until one task completes; timeout returns a snapshot, not proof the task is dead."
    )]
    pub(crate) async fn bro_wait(
        &self,
        Parameters(p): Parameters<WaitParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };

        let caller_token = context.meta.get_progress_token();
        tracing::info!(target: "blackbox::progress", tool = "bro_wait", has_token = caller_token.is_some(), token = ?caller_token, "entry");
        let progress_handle = caller_token.map(|token| {
            spawn_progress_notifier(
                vec![task.clone()],
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        let completed = orch::wait_for_task_with_timeout(&task, p.timeout_seconds).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        let result = if completed {
            orch::task_result_json(&task)
        } else {
            orch::timeout_snapshot_json(&task)
        };
        let mut out = result;
        if let Some(team_ref) =
            orchestration::team::find_bro_ref_for_task(&p.task_id, &self.state.store_dir)
        {
            out["bro"] = Value::String(team_ref.member_name.clone());
            match self
                .maybe_resume_team_advisor(&team_ref.team_name, "wait", &[out.clone()])
                .await
            {
                Ok(Some(value)) => out["advisor"] = value,
                Ok(None) => {}
                Err(err) => out["advisor"] = json!({"error": err}),
            }
        }
        Self::ok_json(&out)
    }

    #[tool(
        name = "bro_when_all",
        description = "Block until ALL tasks / team members complete; use for fan-out/fan-in instead of hand-rolled sequential waits."
    )]
    pub(crate) async fn bro_when_all(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        let progress_handle = context.meta.get_progress_token().map(|token| {
            spawn_progress_notifier(
                tasks.clone(),
                context.peer.clone(),
                token,
                self.state.store_dir.clone(),
            )
        });

        // Wait concurrently (like Promise.all), not sequentially
        let timeout = p.timeout_seconds;
        let store_dir = self.state.store_dir.clone();
        let futs: Vec<_> = tasks
            .iter()
            .map(|task| {
                let task = task.clone();
                let sd = store_dir.clone();
                async move {
                    let completed = orch::wait_for_task_with_timeout(&task, timeout).await;
                    let bro_name = {
                        let inner = task.inner.lock();
                        orchestration::team::find_bro_name_for_task(&inner.id, &sd)
                    };
                    let mut r = if completed {
                        orch::task_result_json(&task)
                    } else {
                        orch::timeout_snapshot_json(&task)
                    };
                    if let Some(name) = bro_name {
                        r["bro"] = Value::String(name);
                    }
                    r
                }
            })
            .collect();

        let results: Vec<Value> = futures::future::join_all(futs).await;
        if let Some(h) = progress_handle {
            h.abort();
        }
        let all_done = results.iter().all(|r| r.get("timed_out").is_none());
        let advisor = match p.team.as_deref() {
            Some(team_name) => {
                self.maybe_resume_team_advisor(team_name, "when_all", &results)
                    .await
            }
            None => Ok(None),
        };
        let mut out = json!({ "all_completed": all_done, "results": results });
        match advisor {
            Ok(Some(value)) => out["advisor"] = value,
            Ok(None) => {}
            Err(err) => out["advisor"] = json!({"error": err}),
        }
        Self::ok_json(&out)
    }

    #[tool(
        name = "bro_when_any",
        description = "Block until the FIRST task completes; use for races instead of polling each task yourself."
    )]
    pub(crate) async fn bro_when_any(
        &self,
        Parameters(p): Parameters<WhenParams>,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> CallToolResult {
        let task_ids = match self.resolve_when_targets(p.team.as_deref(), p.task_ids.as_deref()) {
            Ok(ids) => ids,
            Err(e) => return Self::err_text(&e),
        };

        let tasks: Vec<Arc<orch::Task>> = {
            let store = self.state.task_store.read();
            task_ids.iter().filter_map(|id| store.get(id)).collect()
        };

        // Check if any already done
        let any_done = tasks.iter().any(|t| t.inner.lock().status.is_terminal());
        let progress_handle = if !any_done && !tasks.is_empty() {
            context.meta.get_progress_token().map(|token| {
                spawn_progress_notifier(
                    tasks.clone(),
                    context.peer.clone(),
                    token,
                    self.state.store_dir.clone(),
                )
            })
        } else {
            None
        };

        if !any_done && !tasks.is_empty() {
            // Race them
            let futs: Vec<_> = tasks
                .iter()
                .map(|t| {
                    let t = t.clone();
                    Box::pin(async move {
                        orch::wait_for_task(&t).await;
                    })
                })
                .collect();

            match p.timeout_seconds {
                Some(secs) => {
                    let dur = std::time::Duration::from_secs_f64(secs);
                    let _ = tokio::time::timeout(dur, futures::future::select_all(futs)).await;
                }
                None => {
                    futures::future::select_all(futs).await;
                }
            }
        }
        if let Some(h) = progress_handle {
            h.abort();
        }

        let mut results = Vec::new();
        for task in &tasks {
            let inner = task.inner.lock();
            let bro_name =
                orchestration::team::find_bro_name_for_task(&inner.id, &self.state.store_dir);
            drop(inner);

            let mut r = if task.inner.lock().status.is_terminal() {
                orch::task_result_json(task)
            } else {
                orch::timeout_snapshot_json(task)
            };
            if let Some(name) = bro_name {
                r["bro"] = Value::String(name);
            }
            results.push(r);
        }

        let any_completed = results.iter().any(|r| r.get("timed_out").is_none());
        Self::ok_json(&json!({ "any_completed": any_completed, "results": results }))
    }

    #[tool(
        name = "bro_broadcast",
        description = "Send the same prompt to every team member."
    )]
    pub(crate) async fn bro_broadcast(
        &self,
        Parameters(p): Parameters<BroadcastParams>,
    ) -> CallToolResult {
        let _team_lock = orchestration::team::lock_teams();
        let team = match orchestration::team::load_team(&p.team, &self.state.store_dir) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown team: {}", p.team)),
        };
        let allow_recursion = p.allow_recursion.unwrap_or(false);
        let cwd = p.project_dir.or(team.project_dir.clone());
        let store_dir = self.state.store_dir.clone();
        let mut launched = Vec::new();
        let mut updated_team = team.clone();
        let params_extra =
            extra_filters_from_params(p.allow_tools.as_deref(), p.disallow_tools.as_deref());

        for (i, member) in team.members.iter().enumerate() {
            let brofile = match orchestration::brofile::resolve_brofile(
                &member.brofile,
                &store_dir,
                team.project_dir.as_deref(),
            ) {
                Some(bf) => bf,
                None => {
                    launched.push(json!({"bro": member.name, "error": format!("Brofile not found: {}", member.brofile)}));
                    continue;
                }
            };

            let member_coerce_workspace = brofile.coerce_workspace.unwrap_or(false);
            let env_overrides = orchestration::brofile::resolve_provider_env(
                brofile.provider,
                brofile.account.as_deref(),
                brofile.model.as_deref(),
                &store_dir,
            );
            let exec_opts = if brofile.model.is_some() || brofile.effort.is_some() {
                Some(ExecOpts {
                    model: brofile.model.clone(),
                    effort: brofile.effort.clone(),
                })
            } else {
                None
            };
            // Per-member combined extra: brofile.filters + broadcast-level
            // params overlay. Recursion guard is added inside
            // resolve_dispatch_filters; both layers above merge on top.
            let extra = combine_dispatch_filters(brofile.filters.as_ref(), params_extra.as_ref());

            // Build first-turn prompt with ambient scope + brofile lens.
            // Only applies on fresh-session exec paths; resumes use the
            // raw prompt so ambient/lens aren't re-injected each turn.
            let build_exec_prompt = |task_id: &str, session_id: &str| -> String {
                let ctx = orch::AmbientContext {
                    task_id: Some(task_id.to_string()),
                    session_id: Some(session_id.to_string()),
                    project_dir: cwd.clone(),
                    bro_name: Some(member.name.clone()),
                    thread_id: None,
                    work_item_id: None,
                    pin_block: self.ambient_pin_block(
                        cwd.as_deref(),
                        Some(member.name.as_str()),
                        Some(session_id),
                        None,
                        None,
                    ),
                    completion_contract: if allow_recursion {
                        None
                    } else {
                        Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
                    },
                    allow_recursion,
                    provider: Some(brofile.provider),
                    coerce_workspace: member_coerce_workspace,
                };
                orch::apply_brofile_lens(
                    &orch::apply_ambient(&p.prompt, &ctx),
                    brofile.lens.as_deref(),
                )
            };

            let task = if let Some(ref sid) = member.session_id {
                if sid != "pending" {
                    // Auto-resolve cwd from the session's origin so a
                    // broadcast can resurrect members even when the
                    // current team.project_dir differs from where each
                    // member's session was recorded. Gemini refuses on
                    // miss (silent-fork aliasing); claude/codex fall
                    // through and error loudly themselves.
                    let member_cwd = match brofile.provider.resolve_session_cwd(sid) {
                        Some(p) => Some(p.to_string_lossy().into_owned()),
                        None if brofile.provider == Provider::Gemini => {
                            launched.push(json!({
                                "bro": member.name,
                                "error": format!("Gemini session {sid} not found in ~/.gemini/tmp/*/chats — refusing to resume (silent-fork aliasing)"),
                            }));
                            continue;
                        }
                        None => cwd.clone(),
                    };
                    let task_id = uuid::Uuid::new_v4().to_string();
                    let resume_lease = match try_acquire_resume_lease(
                        &self.state.task_store,
                        self.state.resume_leases.as_ref(),
                        brofile.provider,
                        sid,
                    ) {
                        Ok(lease) => lease,
                        Err(err) => {
                            launched.push(json!({
                                "bro": member.name,
                                "error": err,
                            }));
                            continue;
                        }
                    };
                    let mut args =
                        brofile
                            .provider
                            .build_resume_args(sid, &p.prompt, exec_opts.as_ref());
                    let df = match resolve_dispatch_filters(
                        brofile.provider,
                        member_cwd.as_deref(),
                        allow_recursion,
                        &task_id,
                        extra.as_ref(),
                        None,
                        &self.state.packets.read(),
                    ) {
                        Ok(df) => df,
                        Err(e) => return Self::err_text(&e),
                    };
                    args.extend(df.args);
                    let t = orch::spawn_task(
                        task_id,
                        brofile.provider,
                        args,
                        sid.clone(),
                        member_cwd,
                        env_overrides,
                        store_dir.clone(),
                        self.state.task_store.clone(),
                        self.state.tail_tx.clone(),
                        None,
                        None,
                        Some(self.state.system_events.clone()),
                    );
                    cleanup_policy_file_when_done(t.clone(), df.policy_file);
                    release_resume_lease_when_done(t.clone(), resume_lease);
                    t
                } else {
                    launched.push(json!({
                        "bro": member.name,
                        "error": "Session discovery still pending from the previous launch; refusing to fork a second session",
                    }));
                    continue;
                }
            } else {
                let task_id = uuid::Uuid::new_v4().to_string();
                let session_id = if matches!(brofile.provider, Provider::Claude) {
                    uuid::Uuid::new_v4().to_string()
                } else {
                    "pending".into()
                };
                let exec_prompt = build_exec_prompt(&task_id, &session_id);
                let mut args = brofile.provider.build_exec_args(
                    &exec_prompt,
                    &session_id,
                    cwd.as_deref(),
                    exec_opts.as_ref(),
                );
                let df = match resolve_dispatch_filters(
                    brofile.provider,
                    cwd.as_deref(),
                    allow_recursion,
                    &task_id,
                    extra.as_ref(),
                    None,
                    &self.state.packets.read(),
                ) {
                    Ok(df) => df,
                    Err(e) => return Self::err_text(&e),
                };
                args.extend(df.args);
                let t = orch::spawn_task(
                    task_id,
                    brofile.provider,
                    args,
                    session_id,
                    cwd.clone(),
                    env_overrides,
                    store_dir.clone(),
                    self.state.task_store.clone(),
                    self.state.tail_tx.clone(),
                    None,
                    None,
                    Some(self.state.system_events.clone()),
                );
                cleanup_policy_file_when_done(t.clone(), df.policy_file);
                updated_team.members[i].session_id = Some(t.inner.lock().session_id.clone());
                t
            };

            let tid = task.id();
            updated_team.members[i].task_history.push(tid.clone());
            let sid = task.inner.lock().session_id.clone();
            launched.push(json!({"bro": member.name, "taskId": tid, "sessionId": sid}));
        }

        orchestration::team::save_team(&updated_team, &store_dir);
        Self::ok_json(&json!({"team": p.team, "tasks": launched}))
    }

    #[tool(
        name = "bro_status",
        description = "Non-blocking progress check on a task; call before declaring a timeout dead or cancelling."
    )]
    pub(crate) fn bro_status(&self, Parameters(p): Parameters<StatusParams>) -> CallToolResult {
        match self.state.task_store.read().get(&p.task_id) {
            Some(task) => Self::ok_json(&orch::task_status_json(&task, p.tail.unwrap_or(0))),
            None => Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        }
    }

    #[tool(
        name = "bro_prune",
        description = "Drop terminal tasks from the store + persisted tasks.json."
    )]
    pub(crate) fn bro_prune(&self, Parameters(p): Parameters<PruneParams>) -> CallToolResult {
        let target_status = p.status.as_deref().unwrap_or("failed");
        let allowed = ["failed", "completed", "cancelled"];
        if !allowed.contains(&target_status) {
            return Self::err_text(&format!(
                "status must be one of {:?} (got {:?}); running tasks are never pruned",
                allowed, target_status,
            ));
        }
        let parsed_status: orch::TaskStatus =
            match serde_json::from_str(&format!("\"{target_status}\"")) {
                Ok(s) => s,
                Err(e) => return Self::err_text(&format!("status parse: {e}")),
            };
        let filter_provider = p
            .provider
            .as_deref()
            .and_then(|s| s.parse::<Provider>().ok());
        let cutoff_ms = p
            .older_than_hours
            .map(|h| orch::now_ms().saturating_sub(h.saturating_mul(3600 * 1000)));
        let dry_run = p.dry_run.unwrap_or(false);

        let dropped: Vec<String> = if dry_run {
            self.state
                .task_store
                .read()
                .all_tasks()
                .iter()
                .filter_map(|t| {
                    let inner = t.inner.lock();
                    if inner.status != parsed_status {
                        return None;
                    }
                    if let Some(fp) = filter_provider {
                        if inner.provider != fp {
                            return None;
                        }
                    }
                    if let Some(cutoff) = cutoff_ms {
                        if inner.started_at >= cutoff {
                            return None;
                        }
                    }
                    Some(inner.id.clone())
                })
                .collect()
        } else {
            let mut store = self.state.task_store.write();
            let dropped = store.retain_drop(|t| {
                let inner = t.inner.lock();
                // Keep running tasks always.
                if inner.status == orch::TaskStatus::Running {
                    return true;
                }
                // Keep tasks that don't match the filter.
                if inner.status != parsed_status {
                    return true;
                }
                if let Some(fp) = filter_provider {
                    if inner.provider != fp {
                        return true;
                    }
                }
                if let Some(cutoff) = cutoff_ms {
                    if inner.started_at >= cutoff {
                        return true;
                    }
                }
                false
            });
            store.persist(&self.state.store_dir);
            dropped
        };

        Self::ok_json(&json!({
            "dryRun": dry_run,
            "status": target_status,
            "pruned": dropped.len(),
            "taskIds": dropped,
        }))
    }

    #[tool(
        name = "bro_cancel",
        description = "Cancel a running task (SIGTERM); check bro_status first unless the user explicitly asked to stop."
    )]
    pub(crate) fn bro_cancel(&self, Parameters(p): Parameters<CancelParams>) -> CallToolResult {
        let task = match self.state.task_store.read().get(&p.task_id) {
            Some(t) => t,
            None => return Self::err_text(&format!("Unknown task ID: {}", p.task_id)),
        };
        {
            let inner = task.inner.lock();
            if inner.provider == Provider::Workflow {
                let _ = self.state.cancel_arc(&inner.session_id);
            }
        }
        match orch::cancel_task(&task, &self.state.task_store, &self.state.store_dir) {
            Ok(()) => {
                let inner = task.inner.lock();
                let _ = self.state.tail_tx.send(TailEvent::TaskCancelled {
                    task_id: inner.id.clone(),
                    elapsed: orch::format_elapsed(inner.started_at, inner.completed_at),
                });
                Self::ok_json(&json!({
                    "taskId": inner.id,
                    "sessionId": inner.session_id,
                    "status": "cancelled",
                }))
            }
            Err(e) => Self::err_text(&e),
        }
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn resolve_exec_target(
        &self,
        bro_name: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
            bool,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            match orchestration::team::resolve_bro_selector(name, &teams)? {
                Some(bro_match) => {
                    let member = &bro_match.team.members[bro_match.member_idx];
                    let bf = orchestration::brofile::resolve_brofile(
                        &member.brofile,
                        store_dir,
                        bro_match.team.project_dir.as_deref(),
                    )
                    .ok_or(format!("Brofile not found: {}", member.brofile))?;
                    let env = orchestration::brofile::resolve_provider_env(
                        bf.provider,
                        bf.account.as_deref(),
                        bf.model.as_deref(),
                        store_dir,
                    );
                    let opts = if bf.model.is_some() || bf.effort.is_some() {
                        Some(ExecOpts {
                            model: bf.model.clone(),
                            effort: bf.effort.clone(),
                        })
                    } else {
                        None
                    };
                    let cwd = project_dir
                        .map(String::from)
                        .or(bro_match.team.project_dir.clone());
                    return Ok((
                        bf.provider,
                        bf.lens,
                        opts,
                        env,
                        cwd,
                        bf.filters,
                        bf.coerce_workspace.unwrap_or(false),
                    ));
                }
                None => {
                    // Standalone brofile fallback
                }
            }
            let bf = orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                .ok_or(format!("Unknown bro or brofile: {name}"))?;
            let env = orchestration::brofile::resolve_provider_env(
                bf.provider,
                bf.account.as_deref(),
                bf.model.as_deref(),
                store_dir,
            );
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            return Ok((
                bf.provider,
                bf.lens,
                opts,
                env,
                project_dir.map(String::from),
                bf.filters,
                bf.coerce_workspace.unwrap_or(false),
            ));
        }

        if let Some(p) = raw_provider {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            let env = orchestration::brofile::resolve_provider_env(provider, None, None, store_dir);
            return Ok((
                provider,
                None,
                None,
                env,
                project_dir.map(String::from),
                None,
                false,
            ));
        }

        Err("Provide either bro or provider".into())
    }

    #[allow(clippy::type_complexity)]
    pub(crate) fn resolve_resume_target(
        &self,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        raw_provider: Option<&str>,
        project_dir: Option<&str>,
    ) -> Result<
        (
            Provider,
            String,
            Option<String>,
            Option<ExecOpts>,
            Option<std::collections::HashMap<String, String>>,
            Option<String>,
            Option<orchestration::mcp::McpFilters>,
            bool,
        ),
        String,
    > {
        let store_dir = &self.state.store_dir;

        if let Some(name) = bro_name {
            let teams = orchestration::team::load_all_teams(store_dir);
            let bro_match = orchestration::team::resolve_bro_selector(name, &teams)?
                .ok_or_else(|| {
                    if orchestration::brofile::resolve_brofile(name, store_dir, project_dir)
                        .is_some()
                    {
                        format!(
                            "Brofile \"{name}\" is not in a team — use exec first or provide session_id + provider"
                        )
                    } else {
                        format!("Unknown bro: {name}")
                    }
                })?;
            let member = &bro_match.team.members[bro_match.member_idx];
            let sid = member
                .session_id
                .as_deref()
                .filter(|s| *s != "pending")
                .ok_or(format!(
                    "Bro \"{name}\" has no active session — use exec first"
                ))?;
            let bf = orchestration::brofile::resolve_brofile(
                &member.brofile,
                store_dir,
                bro_match.team.project_dir.as_deref(),
            )
            .ok_or(format!("Brofile not found: {}", member.brofile))?;
            let env = orchestration::brofile::resolve_provider_env(
                bf.provider,
                bf.account.as_deref(),
                bf.model.as_deref(),
                store_dir,
            );
            let opts = if bf.model.is_some() || bf.effort.is_some() {
                Some(ExecOpts {
                    model: bf.model.clone(),
                    effort: bf.effort.clone(),
                })
            } else {
                None
            };
            let cwd = project_dir
                .map(String::from)
                .or(bro_match.team.project_dir.clone());
            return Ok((
                bf.provider,
                sid.to_string(),
                bf.lens,
                opts,
                env,
                cwd,
                bf.filters,
                bf.coerce_workspace.unwrap_or(false),
            ));
        }

        if let (Some(sid), Some(p)) = (session_id, raw_provider) {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            let env = orchestration::brofile::resolve_provider_env(provider, None, None, store_dir);
            return Ok((
                provider,
                sid.to_string(),
                None,
                None,
                env,
                project_dir.map(String::from),
                None,
                false,
            ));
        }

        Err("Provide either bro or session_id + provider".into())
    }

    pub(crate) fn resolve_when_targets(
        &self,
        team_name: Option<&str>,
        task_ids: Option<&[String]>,
    ) -> Result<Vec<String>, String> {
        if let Some(name) = team_name {
            let team = orchestration::team::load_team(name, &self.state.store_dir)
                .ok_or(format!("Unknown team: {name}"))?;
            let ids: Vec<String> = team
                .members
                .iter()
                .filter_map(|m| m.task_history.last().cloned())
                .collect();
            if ids.is_empty() {
                return Err(format!("No tasks found for team {name}"));
            }
            return Ok(ids);
        }
        if let Some(ids) = task_ids {
            if ids.is_empty() {
                return Err("Empty task_ids array".into());
            }
            return Ok(ids.to_vec());
        }
        Err("Provide either team or task_ids".into())
    }
}

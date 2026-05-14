use crate::server::*;
use crate::*;

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::dispatch_tools()
}

pub(crate) struct FreshDispatchRequest {
    pub(crate) prompt: String,
    pub(crate) provider: Provider,
    pub(crate) lens: Option<String>,
    pub(crate) exec_opts: Option<orchestration::providers::ExecOpts>,
    pub(crate) env_overrides: Option<std::collections::HashMap<String, String>>,
    pub(crate) cwd: Option<String>,
    pub(crate) brofile_filters: Option<orchestration::mcp::McpFilters>,
    pub(crate) coerce_workspace: bool,
    pub(crate) allow_recursion: bool,
    pub(crate) allow_tools: Option<Vec<String>>,
    pub(crate) disallow_tools: Option<Vec<String>>,
    pub(crate) surface: Option<String>,
    pub(crate) allocation_request: Option<orchestration::allocator::RuntimeRequest>,
    pub(crate) project_dir_for_lease: Option<String>,
    pub(crate) ambient_bro_name: Option<String>,
    pub(crate) spawn_bro_label: Option<String>,
    pub(crate) spawn_agent_label: Option<String>,
    pub(crate) record_to_bro: Option<String>,
}

pub(crate) struct FreshDispatchResult {
    pub(crate) task: std::sync::Arc<orch::Task>,
    pub(crate) allocation: Option<orchestration::allocator::Allocation>,
}

fn exec_params_have_runtime(p: &ExecParams) -> bool {
    p.tier.is_some()
        || p.tier_ladder.is_some()
        || p.tier_mode.is_some()
        || p.min_tier.is_some()
        || p.max_tier.is_some()
        || p.pool_name.is_some()
        || p.pool_providers.as_ref().is_some_and(|v| !v.is_empty())
        || p.pin_provider.is_some()
        || p.pin_account.is_some()
        || p.pin_model.is_some()
        || p.pin_effort.is_some()
        || p.prefer_provider.is_some()
        || p.capabilities.as_ref().is_some_and(|v| !v.is_empty())
        || p.durable.is_some()
        || p.selection_policy.is_some()
}

fn exec_params_runtime_request(
    p: &ExecParams,
    base: Option<orchestration::allocator::RuntimeRequest>,
) -> Result<Option<orchestration::allocator::RuntimeRequest>, String> {
    if !exec_params_have_runtime(p) && base.is_none() {
        return Ok(None);
    }
    let mut request = base.unwrap_or_default();
    if let Some(tier) = p.tier.clone() {
        request.tier = Some(tier);
    }
    if request.tier.is_none()
        && (p.tier_ladder.is_some()
            || p.tier_mode.is_some()
            || p.min_tier.is_some()
            || p.max_tier.is_some())
    {
        return Err(
            "error.bad_allocation_request: tier_ladder, tier_mode, min_tier, and max_tier require tier".into(),
        );
    }
    if let Some(ladder) = p.tier_ladder.clone() {
        request.tier_ladder = Some(ladder);
    }
    if let Some(mode) = p.tier_mode.as_deref() {
        request.tier_mode = match mode {
            "exact" => orchestration::allocator::TierMode::Exact,
            "at_least" => orchestration::allocator::TierMode::AtLeast,
            "bounded" => orchestration::allocator::TierMode::Bounded,
            other => {
                return Err(format!(
                    "error.bad_allocation_request: unknown tier_mode `{other}`"
                ));
            }
        };
    }
    if let Some(min_tier) = p.min_tier.clone() {
        request.min_tier = Some(min_tier);
    }
    if let Some(max_tier) = p.max_tier.clone() {
        request.max_tier = Some(max_tier);
    }
    if p.pool_name.is_some() || p.pool_providers.as_ref().is_some_and(|v| !v.is_empty()) {
        let mut pool = request.pool.take().unwrap_or_default();
        if let Some(name) = p.pool_name.clone() {
            pool.name = Some(name);
        }
        if let Some(providers) = &p.pool_providers {
            pool.providers = providers
                .iter()
                .map(|provider| {
                    provider
                        .parse::<Provider>()
                        .map_err(|_| format!("Unknown provider: {provider}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
        }
        request.pool = Some(pool);
    }
    if let Some(capabilities) = &p.capabilities {
        request.capabilities = orchestration::allocator::parse_capabilities(capabilities)?;
    }
    if p.pin_provider.is_some()
        || p.pin_account.is_some()
        || p.pin_model.is_some()
        || p.pin_effort.is_some()
    {
        let pin = request.pin.get_or_insert_with(Default::default);
        if let Some(provider) = p.pin_provider.as_deref() {
            pin.provider = Some(
                provider
                    .parse::<Provider>()
                    .map_err(|_| format!("Unknown provider: {provider}"))?,
            );
        }
        if let Some(account) = p.pin_account.clone() {
            pin.account = Some(account);
        }
        if let Some(model) = p.pin_model.clone() {
            pin.model = Some(model);
        }
        if let Some(effort) = p.pin_effort.clone() {
            pin.effort = Some(effort);
        }
        pin.authority = orchestration::allocator::PinAuthority::Operator;
    }
    if let Some(provider) = p.prefer_provider.as_deref() {
        request.prefer = Some(orchestration::allocator::RuntimePreference {
            provider: Some(
                provider
                    .parse::<Provider>()
                    .map_err(|_| format!("Unknown provider: {provider}"))?,
            ),
        });
    }
    if let Some(durable) = p.durable {
        request.durable = durable;
    } else if exec_params_have_runtime(p) {
        request.durable = true;
    }
    if p.surface.is_some()
        || p.allow_tools.as_ref().is_some_and(|v| !v.is_empty())
        || p.disallow_tools.as_ref().is_some_and(|v| !v.is_empty())
        || p.coerce_workspace == Some(true)
    {
        request
            .derived_capabilities
            .push(orchestration::providers::Capability::ToolUse);
        request
            .derived_capabilities
            .sort_by_key(|cap| format!("{cap:?}"));
        request.derived_capabilities.dedup();
    }
    if let Some(policy) = p.selection_policy.clone() {
        request.selection_policy = Some(policy);
    }
    Ok(Some(request))
}

fn allocator_status_runtime_request(
    p: &AllocatorStatusParams,
) -> Result<Option<orchestration::allocator::RuntimeRequest>, String> {
    let exec_like = ExecParams {
        prompt: String::new(),
        bro: None,
        provider: None,
        project_dir: p.project_dir.clone(),
        allow_recursion: None,
        allow_tools: None,
        disallow_tools: None,
        surface: None,
        coerce_workspace: None,
        tier: p.tier.clone(),
        tier_ladder: p.tier_ladder.clone(),
        tier_mode: p.tier_mode.clone(),
        min_tier: p.min_tier.clone(),
        max_tier: p.max_tier.clone(),
        pool_name: p.pool_name.clone(),
        pool_providers: p.pool_providers.clone(),
        pin_provider: p.pin_provider.clone(),
        pin_account: p.pin_account.clone(),
        pin_model: p.pin_model.clone(),
        pin_effort: p.pin_effort.clone(),
        prefer_provider: p.prefer_provider.clone(),
        capabilities: p.capabilities.clone(),
        durable: p.durable,
        selection_policy: p.selection_policy.clone(),
    };
    exec_params_runtime_request(&exec_like, None)
}

fn parse_allocator_probe_enum<T>(field: &str, value: &Option<String>) -> Result<Option<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    value
        .as_ref()
        .map(|value| {
            serde_json::from_value::<T>(serde_json::Value::String(value.clone()))
                .map_err(|_| format!("error.bad_probe_state: invalid {field} value `{value}`"))
        })
        .transpose()
}

impl BlackboxServer {
    pub(crate) fn dispatch_fresh_bro_task(
        &self,
        mut request: FreshDispatchRequest,
    ) -> Result<FreshDispatchResult, String> {
        let store_dir = self.state.store_dir.clone();
        let _allocation_guard = request
            .allocation_request
            .as_ref()
            .map(|_| orchestration::allocator::allocation_lock());
        let mut allocation: Option<orchestration::allocator::Allocation> = None;
        if let Some(runtime_request) = request.allocation_request.take() {
            let allocator_config =
                orchestration::allocator::load_effective_config(&store_dir, request.cwd.as_deref());
            let bro_config = orchestration::brofile::load_config(&store_dir);
            let lease_store = orchestration::allocator::lease_store_load(&store_dir);
            let probe_store = orchestration::allocator::probe_store_load(&store_dir);
            let ctx = orchestration::allocator::allocation_context_with_probes(
                &self.state.task_store.read(),
                &lease_store,
                probe_store,
            );
            let selected = orchestration::allocator::allocate(
                runtime_request,
                &allocator_config,
                &bro_config,
                &ctx,
            );
            orchestration::allocator::save_trace(&store_dir, &selected.trace);
            if let Some(err) = selected.trace.error.as_deref() {
                return Err(err.to_string());
            }
            request.provider = selected.lane.provider;
            request.exec_opts = orchestration::allocator::exec_opts_for_lane(&selected.lane);
            request.env_overrides = orchestration::brofile::resolve_provider_env(
                request.provider,
                selected.lane.account.as_deref(),
                selected.lane.model.as_deref(),
                &store_dir,
            );
            allocation = Some(selected);
        }

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if matches!(request.provider, Provider::Claude) {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: request.cwd.clone(),
            bro_name: request.ambient_bro_name.clone(),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                request.cwd.as_deref(),
                request.ambient_bro_name.as_deref(),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: if request.allow_recursion {
                None
            } else {
                Some(orch::DEFAULT_COMPLETION_CONTRACT.to_string())
            },
            allow_recursion: request.allow_recursion,
            provider: Some(request.provider),
            coerce_workspace: request.coerce_workspace,
        };
        let final_prompt = orch::apply_brofile_lens(
            &orch::apply_ambient(&request.prompt, &ambient_ctx),
            request.lens.as_deref(),
        );
        let mut args = request.provider.build_exec_args(
            &final_prompt,
            &session_id,
            request.cwd.as_deref(),
            request.exec_opts.as_ref(),
        );
        let params_extra = extra_filters_from_params(
            request.allow_tools.as_deref(),
            request.disallow_tools.as_deref(),
        );
        let extra =
            combine_dispatch_filters(request.brofile_filters.as_ref(), params_extra.as_ref());
        let dispatch_filters = resolve_dispatch_filters(
            request.provider,
            request.cwd.as_deref(),
            request.allow_recursion,
            &task_id,
            extra.as_ref(),
            request.surface.as_deref(),
            &self.state.packets.read(),
        )
        .map_err(|e| e.to_string())?;
        args.extend(dispatch_filters.args);

        let task = orch::spawn_task(
            task_id,
            request.provider,
            args,
            session_id,
            request.cwd,
            request.env_overrides,
            store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            request.spawn_bro_label,
            request.spawn_agent_label,
            Some(self.state.system_events.clone()),
        );
        if let Some(allocation) = &allocation {
            orchestration::allocator::record_lease(
                &store_dir,
                orchestration::allocator::lease_from_allocation(
                    task.inner.lock().id.clone(),
                    task.inner.lock().session_id.clone(),
                    allocation,
                    request.project_dir_for_lease,
                    task.inner.lock().cwd.clone(),
                ),
            );
        }
        cleanup_policy_file_when_done(task.clone(), dispatch_filters.policy_file);
        if let Some(bro_name) = &request.record_to_bro {
            self.record_task_to_bro(bro_name, &task);
        }
        Ok(FreshDispatchResult { task, allocation })
    }
}

#[tool_router(router = dispatch_tools)]
impl BlackboxServer {
    #[tool(
        name = "bro_exec",
        description = "Launch a fresh agent task/session and return {taskId, sessionId}; can target a named bro/provider or request runtime allocation by tier/pool/capabilities."
    )]
    pub(crate) async fn bro_exec(&self, Parameters(p): Parameters<ExecParams>) -> CallToolResult {
        let allow_recursion = p.allow_recursion.unwrap_or(false);

        let (
            provider,
            lens,
            exec_opts,
            env_overrides,
            cwd,
            brofile_filters,
            brofile_coerce_workspace,
        ) = if p.bro.is_some() || p.provider.is_some() {
            match self.resolve_exec_target(
                p.bro.as_deref(),
                p.provider.as_deref(),
                p.project_dir.as_deref(),
            ) {
                Ok(r) => r,
                Err(e) => return Self::err_text(&e),
            }
        } else if exec_params_have_runtime(&p) {
            (
                Provider::Codex,
                None,
                None,
                None,
                p.project_dir.clone(),
                None,
                false,
            )
        } else {
            return Self::err_text(
                "Provide either bro, provider, or runtime allocation parameters",
            );
        };
        let brofile_runtime = p
            .bro
            .as_deref()
            .and_then(|name| {
                self.resolve_exec_brofile_for_allocator(name, p.project_dir.as_deref())
            })
            .and_then(|bf| bf.runtime);
        let mut allocation_request = match exec_params_runtime_request(&p, brofile_runtime) {
            Ok(request) => request,
            Err(err) => return Self::err_text(&err),
        };
        if let Some(request) = &mut allocation_request {
            if let Some(raw_provider) = p.provider.as_deref() {
                let pinned_provider = match raw_provider.parse::<Provider>() {
                    Ok(provider) => provider,
                    Err(_) => return Self::err_text(&format!("Unknown provider: {raw_provider}")),
                };
                let pin = request.pin.get_or_insert_with(Default::default);
                pin.provider = Some(pinned_provider);
                pin.authority = orchestration::allocator::PinAuthority::Operator;
            }
        }
        let coerce_workspace = p.coerce_workspace.unwrap_or(brofile_coerce_workspace);
        let dispatched = match self.dispatch_fresh_bro_task(FreshDispatchRequest {
            prompt: p.prompt.clone(),
            provider,
            lens,
            exec_opts,
            env_overrides,
            cwd: cwd.clone(),
            brofile_filters,
            coerce_workspace,
            allow_recursion,
            allow_tools: p.allow_tools.clone(),
            disallow_tools: p.disallow_tools.clone(),
            surface: p.surface.clone(),
            allocation_request,
            project_dir_for_lease: p.project_dir.clone(),
            ambient_bro_name: p.bro.clone(),
            spawn_bro_label: None,
            spawn_agent_label: None,
            record_to_bro: p.bro.clone(),
        }) {
            Ok(result) => result,
            Err(e) => return Self::err_text(&e),
        };

        let inner = dispatched.task.inner.lock();
        let mut response = json!({
            "taskId": inner.id,
            "sessionId": inner.session_id,
            "status": "running",
        });
        if let Some(allocation) = &dispatched.allocation {
            response["provider"] = json!(allocation.lane.provider);
            response["account"] = json!(allocation.lane.account);
            response["model"] = json!(allocation.lane.model);
            response["effort"] = json!(allocation.lane.effort);
            response["tier"] = json!(allocation.lane.tier);
            response["selectionTraceId"] = json!(allocation.trace.id);
        }
        Self::ok_json(&response)
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
            runtime_lease,
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
            None if provider == Provider::Gemini && cwd.is_none() => {
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
            store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            None,
            None,
            Some(self.state.system_events.clone()),
        );
        if let Some(lease) = &runtime_lease {
            let inner = task.inner.lock();
            orchestration::allocator::record_lease(
                &store_dir,
                orchestration::allocator::lease_for_resume_task(
                    lease,
                    inner.id.clone(),
                    inner.session_id.clone(),
                    inner.cwd.clone(),
                ),
            );
        }
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
        name = "bro_allocator_status",
        description = "Read pool-backed runtime allocation config, active leases, in-flight lane counts, and optional candidate preview."
    )]
    pub(crate) fn bro_allocator_status(
        &self,
        Parameters(p): Parameters<AllocatorStatusParams>,
    ) -> CallToolResult {
        let cfg = orchestration::allocator::load_effective_config(
            &self.state.store_dir,
            p.project_dir.as_deref(),
        );
        let leases = orchestration::allocator::lease_store_load(&self.state.store_dir);
        let probes = orchestration::allocator::probe_store_load(&self.state.store_dir);
        let ctx = orchestration::allocator::allocation_context_with_probes(
            &self.state.task_store.read(),
            &leases,
            probes.clone(),
        );
        let preview_request = match allocator_status_runtime_request(&p) {
            Ok(request) => request,
            Err(err) => return Self::err_text(&err),
        };
        let preview = preview_request.map(|request| {
            let bro_config = orchestration::brofile::load_config(&self.state.store_dir);
            let allocation =
                orchestration::allocator::allocate(request, &cfg, &bro_config, &ctx);
            let now = orchestration::now_ms();
            let candidates: Vec<_> = allocation
                .trace
                .candidates
                .iter()
                .map(|candidate| {
                    let lane_key = format!(
                        "{}:{}",
                        candidate.lane.provider.as_str(),
                        candidate.lane.account.as_deref().unwrap_or("default")
                    );
                    let probe = ctx.probes.get(&lane_key);
                    let probe_observed_at = probe.and_then(|probe| {
                        probe
                            .last_probe_at
                            .into_iter()
                            .chain(probe.last_runtime_observation_at)
                            .max()
                    });
                    json!({
                        "lane": &candidate.lane,
                        "lane_key": lane_key,
                        "eligible": candidate.eligible,
                        "exclusion_reason": &candidate.exclusion_reason,
                        "score": candidate.score,
                        "score_components": &candidate.score_components,
                        "in_flight": ctx.in_flight.get(&lane_key).copied().unwrap_or(0),
                        "probe": probe,
                        "credential_status": probe.map(|probe| &probe.credential_status),
                        "quota_status": probe.map(|probe| &probe.quota_status),
                        "quota_confidence": probe.map(|probe| &probe.quota_confidence),
                        "cooldown_until": probe.and_then(|probe| probe.cooldown_until),
                        "cooldown_active": probe.and_then(|probe| probe.cooldown_until).is_some_and(|until| until > now),
                        "probe_observed_at": probe_observed_at,
                        "probe_staleness_ms": probe_observed_at.map(|observed| now.saturating_sub(observed)),
                    })
                })
                .collect();
            json!({
                "trace_id": &allocation.trace.id,
                "request": &allocation.trace.request,
                "candidate_tiers": &allocation.trace.candidate_tiers,
                "required_capabilities": &allocation.trace.required_capabilities,
                "selected": &allocation.trace.selected,
                "error": &allocation.trace.error,
                "candidates": candidates,
            })
        });
        Self::ok_json(&json!({
            "tiers": cfg.tiers,
            "tier_ladders": cfg.tier_ladders,
            "pools": cfg.pools,
            "selection_policies": cfg.selection_policies,
            "in_flight": ctx.in_flight,
            "probes": probes.records,
            "leases": leases.leases,
            "preview": preview,
        }))
    }

    #[tool(
        name = "bro_allocator_trace",
        description = "Read a previous runtime allocation selection trace by id."
    )]
    pub(crate) fn bro_allocator_trace(
        &self,
        Parameters(p): Parameters<AllocatorTraceParams>,
    ) -> CallToolResult {
        match orchestration::allocator::load_trace(&self.state.store_dir, &p.selection_trace_id) {
            Some(trace) => {
                Self::ok_json(&serde_json::to_value(trace).unwrap_or_else(|_| json!({})))
            }
            None => Self::err_text(&format!(
                "Unknown allocation trace: {}",
                p.selection_trace_id
            )),
        }
    }

    #[tool(
        name = "bro_allocator_probe",
        description = "Read, update, or clear allocator probe state for a provider/account lane."
    )]
    pub(crate) fn bro_allocator_probe(
        &self,
        Parameters(p): Parameters<AllocatorProbeParams>,
    ) -> CallToolResult {
        let provider = match p.provider.parse::<Provider>() {
            Ok(provider) => provider,
            Err(_) => return Self::err_text(&format!("Unknown provider: {}", p.provider)),
        };
        let lane_key = format!(
            "{}:{}",
            provider.as_str(),
            p.account.as_deref().unwrap_or("default")
        );
        let mut probes = orchestration::allocator::probe_store_load(&self.state.store_dir);
        if p.clear == Some(true) {
            let removed = probes.records.remove(&lane_key);
            orchestration::allocator::probe_store_save(&self.state.store_dir, &probes);
            return Self::ok_json(&json!({
                "lane_key": lane_key,
                "cleared": removed.is_some(),
                "probe": serde_json::Value::Null,
            }));
        }

        let credential_status = match parse_allocator_probe_enum::<
            orchestration::allocator::CredentialStatus,
        >("credential_status", &p.credential_status)
        {
            Ok(status) => status,
            Err(err) => return Self::err_text(&err),
        };
        let quota_status = match parse_allocator_probe_enum::<orchestration::allocator::QuotaStatus>(
            "quota_status",
            &p.quota_status,
        ) {
            Ok(status) => status,
            Err(err) => return Self::err_text(&err),
        };
        let quota_confidence = match parse_allocator_probe_enum::<
            orchestration::allocator::QuotaConfidence,
        >("quota_confidence", &p.quota_confidence)
        {
            Ok(confidence) => confidence,
            Err(err) => return Self::err_text(&err),
        };
        let has_update = credential_status.is_some()
            || quota_status.is_some()
            || quota_confidence.is_some()
            || p.five_hour_utilization.is_some()
            || p.seven_day_utilization.is_some()
            || p.balance_capacity.is_some()
            || p.cooldown_until.is_some()
            || p.cooldown_ms.is_some()
            || p.raw_summary.is_some();
        if has_update {
            let now = orchestration::now_ms();
            let record = probes.records.entry(lane_key.clone()).or_insert_with(|| {
                orchestration::allocator::ProbeRecord {
                    provider,
                    account: p.account.clone(),
                    credential_status: Default::default(),
                    quota_status: Default::default(),
                    quota_confidence: Default::default(),
                    five_hour_utilization: None,
                    seven_day_utilization: None,
                    balance_capacity: None,
                    cooldown_until: None,
                    last_probe_at: None,
                    last_runtime_observation_at: None,
                    raw_summary: None,
                }
            });
            if let Some(status) = credential_status {
                record.credential_status = status;
            }
            if let Some(status) = quota_status {
                record.quota_status = status;
            }
            if let Some(confidence) = quota_confidence {
                record.quota_confidence = confidence;
            }
            if p.five_hour_utilization.is_some() {
                record.five_hour_utilization = p.five_hour_utilization;
            }
            if p.seven_day_utilization.is_some() {
                record.seven_day_utilization = p.seven_day_utilization;
            }
            if p.balance_capacity.is_some() {
                record.balance_capacity = p.balance_capacity;
            }
            if let Some(until) = p.cooldown_until {
                record.cooldown_until = Some(until);
            } else if let Some(ms) = p.cooldown_ms {
                record.cooldown_until = Some(now.saturating_add(ms));
            }
            if p.raw_summary.is_some() {
                record.raw_summary = p.raw_summary;
            }
            record.last_probe_at = Some(now);
            orchestration::allocator::probe_store_save(&self.state.store_dir, &probes);
        }
        let probe = probes.records.get(&lane_key).cloned();
        Self::ok_json(&json!({
            "lane_key": lane_key,
            "probe": probe,
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
    pub(crate) fn resolve_exec_brofile_for_allocator(
        &self,
        bro_name: &str,
        project_dir: Option<&str>,
    ) -> Option<orchestration::brofile::Brofile> {
        let store_dir = &self.state.store_dir;
        let teams = orchestration::team::load_all_teams(store_dir);
        if let Ok(Some(bro_match)) = orchestration::team::resolve_bro_selector(bro_name, &teams) {
            let member = &bro_match.team.members[bro_match.member_idx];
            return orchestration::brofile::resolve_brofile(
                &member.brofile,
                store_dir,
                bro_match.team.project_dir.as_deref(),
            );
        }
        orchestration::brofile::resolve_brofile(bro_name, store_dir, project_dir)
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
            Option<orchestration::allocator::RuntimeLease>,
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
            let lease = member.task_history.last().and_then(|task_id| {
                orchestration::allocator::lookup_lease_for_task(store_dir, task_id)
            });
            let (provider, opts, env) = if let Some(lease) = lease.as_ref() {
                let provider = lease.provider;
                let opts = orchestration::allocator::exec_opts_for_lane(
                    &orchestration::allocator::RuntimeLane {
                        provider,
                        account: lease.account.clone(),
                        tier: lease.tier.clone(),
                        model: lease.model.clone(),
                        effort: lease.effort.clone(),
                        capabilities: lease.capabilities.clone(),
                    },
                );
                let env = orchestration::brofile::resolve_provider_env(
                    provider,
                    lease.account.as_deref(),
                    lease.model.as_deref(),
                    store_dir,
                );
                (provider, opts, env)
            } else {
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
                (bf.provider, opts, env)
            };
            let cwd = project_dir
                .map(String::from)
                .or(bro_match.team.project_dir.clone());
            return Ok((
                provider,
                sid.to_string(),
                bf.lens,
                opts,
                env,
                cwd,
                bf.filters,
                bf.coerce_workspace.unwrap_or(false),
                lease,
            ));
        }

        if let (Some(sid), Some(p)) = (session_id, raw_provider) {
            let provider = p
                .parse::<Provider>()
                .map_err(|_| format!("Unknown provider: {p}"))?;
            if let Some(lease) = orchestration::allocator::lookup_lease_for_session(
                store_dir,
                &self.state.task_store.read(),
                provider,
                sid,
            ) {
                let env = orchestration::brofile::resolve_provider_env(
                    provider,
                    lease.account.as_deref(),
                    lease.model.as_deref(),
                    store_dir,
                );
                let opts = orchestration::allocator::exec_opts_for_lane(
                    &orchestration::allocator::RuntimeLane {
                        provider,
                        account: lease.account.clone(),
                        tier: lease.tier.clone(),
                        model: lease.model.clone(),
                        effort: lease.effort.clone(),
                        capabilities: lease.capabilities.clone(),
                    },
                );
                return Ok((
                    provider,
                    sid.to_string(),
                    None,
                    opts,
                    env,
                    project_dir
                        .map(String::from)
                        .or_else(|| lease.cwd.clone())
                        .or_else(|| lease.project_dir.clone()),
                    None,
                    false,
                    Some(lease),
                ));
            }
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
                None,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn params() -> ExecParams {
        ExecParams {
            prompt: "test".into(),
            bro: None,
            provider: None,
            project_dir: None,
            allow_recursion: None,
            allow_tools: None,
            disallow_tools: None,
            surface: None,
            coerce_workspace: None,
            tier: None,
            tier_ladder: None,
            tier_mode: None,
            min_tier: None,
            max_tier: None,
            pool_name: None,
            pool_providers: None,
            pin_provider: None,
            pin_account: None,
            pin_model: None,
            pin_effort: None,
            prefer_provider: None,
            capabilities: None,
            durable: None,
            selection_policy: None,
        }
    }

    #[test]
    fn exec_params_runtime_request_parses_operator_pins_and_preferences() {
        let mut params = params();
        params.pin_provider = Some("codex".into());
        params.pin_account = Some("codex-alt".into());
        params.pin_model = Some("gpt-5.3-codex-spark".into());
        params.pin_effort = Some("low".into());
        params.prefer_provider = Some("glm".into());
        assert!(exec_params_have_runtime(&params));
        let request = exec_params_runtime_request(&params, None).unwrap().unwrap();
        let pin = request.pin.unwrap();
        assert_eq!(pin.provider, Some(Provider::Codex));
        assert_eq!(pin.account.as_deref(), Some("codex-alt"));
        assert_eq!(pin.model.as_deref(), Some("gpt-5.3-codex-spark"));
        assert_eq!(pin.effort.as_deref(), Some("low"));
        assert_eq!(
            pin.authority,
            orchestration::allocator::PinAuthority::Operator
        );
        assert_eq!(
            request.prefer.and_then(|prefer| prefer.provider),
            Some(Provider::Glm)
        );
    }

    #[test]
    fn exec_params_runtime_request_rejects_unknown_pin_provider() {
        let mut params = params();
        params.pin_provider = Some("not-a-provider".into());
        let err = exec_params_runtime_request(&params, None).unwrap_err();
        assert!(err.contains("Unknown provider: not-a-provider"), "{err}");
    }

    #[test]
    fn exec_params_runtime_request_rejects_unknown_preferred_provider() {
        let mut params = params();
        params.prefer_provider = Some("not-a-provider".into());
        let err = exec_params_runtime_request(&params, None).unwrap_err();
        assert!(err.contains("Unknown provider: not-a-provider"), "{err}");
    }

    #[test]
    fn exec_params_runtime_request_derives_tool_use_from_tool_surface() {
        let mut params = params();
        params.tier = Some("standard".into());
        params.surface = Some("readonly".into());
        let request = exec_params_runtime_request(&params, None).unwrap().unwrap();
        assert!(
            request
                .derived_capabilities
                .contains(&orchestration::providers::Capability::ToolUse)
        );
    }

    #[test]
    fn allocator_status_runtime_request_matches_exec_allocation_fields() {
        let params = AllocatorStatusParams {
            project_dir: None,
            tier: Some("standard".into()),
            tier_ladder: Some("default".into()),
            tier_mode: Some("at_least".into()),
            min_tier: None,
            max_tier: None,
            pool_name: Some("coding".into()),
            pool_providers: Some(vec!["codex".into()]),
            pin_provider: Some("codex".into()),
            pin_account: Some("codex-alt".into()),
            pin_model: Some("gpt-5.3-codex-spark".into()),
            pin_effort: Some("low".into()),
            prefer_provider: Some("codex".into()),
            capabilities: Some(vec!["tool_use".into()]),
            durable: Some(false),
            selection_policy: Some(serde_json::json!("availability")),
        };
        let request = allocator_status_runtime_request(&params).unwrap().unwrap();
        assert_eq!(request.tier.as_deref(), Some("standard"));
        assert_eq!(request.tier_ladder.as_deref(), Some("default"));
        assert_eq!(
            request.tier_mode,
            orchestration::allocator::TierMode::AtLeast
        );
        assert_eq!(
            request.pool.unwrap().providers,
            vec![orchestration::providers::Provider::Codex]
        );
        assert_eq!(
            request.pin.as_ref().and_then(|pin| pin.provider),
            Some(orchestration::providers::Provider::Codex)
        );
        assert_eq!(
            request.pin.as_ref().and_then(|pin| pin.account.as_deref()),
            Some("codex-alt")
        );
        assert_eq!(
            request.prefer.and_then(|prefer| prefer.provider),
            Some(Provider::Codex)
        );
        assert!(!request.durable);
        assert!(
            request
                .capabilities
                .contains(&orchestration::providers::Capability::ToolUse)
        );
    }
}

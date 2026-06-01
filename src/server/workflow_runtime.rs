use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::orchestration;
use crate::orchestration as orch;
use crate::orchestration::providers::Provider;
use crate::packets::{self, apply_with as apply_packet_with};
use crate::pins::AmbientPinQuery;
use crate::server::BlackboxServer;
use crate::server::progress::{
    cleanup_policy_file_when_done, release_resume_lease_when_done, resolve_dispatch_filters,
    try_acquire_resume_lease,
};
use crate::workflow;

impl BlackboxServer {
    pub(crate) fn ambient_pin_block(
        &self,
        project_dir: Option<&str>,
        bro_name: Option<&str>,
        session_id: Option<&str>,
        thread_id: Option<&str>,
        work_item_id: Option<&str>,
    ) -> Option<String> {
        self.state.pins.read().render_for_ambient(&AmbientPinQuery {
            project: project_dir,
            bro: bro_name,
            session_id,
            thread_id,
            work_item_id,
        })
    }

    /// Dispatch an executor node turn through a provider's interactive TUI in
    /// a tmux pane (`terminal_mode=tmux`). The turn is driven by
    /// `tmux_dispatch::run_terminal_turn` and resolved from the transcript read
    /// plane; this returns an already-terminal synthetic `Task` whose result is
    /// the assistant text, so the engine's normal wait/result/session handling
    /// applies unchanged. MVP: fresh session per visit (no durable resume yet);
    /// the in-flight turn is not interrupted by arc cancel (bounded by the
    /// turn timeout). See the tmux terminal-mode slice design.
    pub(crate) async fn workflow_dispatch_executor_tmux(
        &self,
        brofile: &str,
        prompt: &str,
        project_dir: Option<&str>,
        arc_id: &str,
        actor_label: &str,
        existing_session: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<Arc<orch::Task>, String> {
        use crate::orchestration::tmux::CliTmuxBackend;
        use crate::orchestration::tmux_dispatch::{
            TerminalTurnConfig, TerminalTurnTiming, run_terminal_turn,
        };
        use crate::transcripts::adapters::TranscriptAdapterRegistry;

        let (provider, _lens, exec_opts, _env, cwd, _filters, _coerce, _ctx) =
            self.resolve_exec_target(Some(brofile), None, project_dir)?;
        if !provider.tui_capable() {
            return Err(format!(
                "actor brofile '{brofile}' resolves to provider {provider}, which is not \
                 TUI-capable; terminal_mode=tmux requires Claude or Codex"
            ));
        }
        let cwd_path = cwd
            .clone()
            .or_else(|| project_dir.map(str::to_string))
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());

        let cfg = TerminalTurnConfig {
            provider,
            arc_id: arc_id.to_string(),
            actor_label: actor_label.to_string(),
            prompt: prompt.to_string(),
            cwd: cwd_path,
            model: exec_opts.as_ref().and_then(|o| o.model.clone()),
            effort: exec_opts.as_ref().and_then(|o| o.effort.clone()),
        };
        let config = self.state.idx.read().reindex_config();
        let registry = TranscriptAdapterRegistry::from_reindex_config(&config);
        let backend = CliTmuxBackend::new();

        let id = format!("tmux-{}", uuid::Uuid::new_v4());
        let cwd_str = cfg.cwd.to_string_lossy().to_string();
        let task = match run_terminal_turn(
            &backend,
            &registry,
            &cfg,
            &TerminalTurnTiming::default(),
            existing_session,
            cancel,
        )
        .await
        {
            Ok(outcome) => {
                // No portal yet, so the turn's window is dead weight after
                // completion. Reap the whole container session (actor window +
                // tmux's default placeholder) so nothing lingers; the next
                // durable turn recreates it via ensure_session. (The error path
                // inside run_terminal_turn reaps too.)
                use crate::orchestration::tmux::TmuxBackend;
                let _ = backend.kill_session(&outcome.handle.session).await;
                orch::synthetic_terminal_task(
                    id.clone(),
                    provider,
                    outcome.session_id,
                    orch::TaskStatus::Completed,
                    Some(outcome.assistant_text),
                    String::new(),
                    Some(cwd_str),
                    Some(outcome.location),
                )
            }
            // Cancellation is not a task failure — propagate it so the runner
            // tears the arc down rather than recording a failed node.
            Err(_) if cancel.is_cancelled() => {
                return Err("terminal-mode turn cancelled by arc cancel".to_string());
            }
            Err(e) => orch::synthetic_terminal_task(
                id.clone(),
                provider,
                String::new(),
                orch::TaskStatus::Failed,
                None,
                format!("terminal-mode dispatch failed: {e}"),
                Some(cwd_str),
                None,
            ),
        };
        self.state
            .task_store
            .write()
            .insert(id.clone(), task.clone())
            .map_err(|e| format!("insert terminal task: {e}"))?;
        self.state.task_store.read().persist(&self.state.store_dir);
        Ok(task)
    }

    /// Dispatch an executor node's turn (new session or resume of an
    /// existing one). Returns the spawned `Task` so the caller can wait
    /// on it. Duplicates the core of `bro_exec` / `bro_resume` minus the
    /// MCP-result formatting — used by the workflow engine.
    pub(crate) async fn workflow_dispatch_executor(
        &self,
        brofile: &str,
        prompt: &str,
        project_dir: Option<&str>,
        existing_session_id: Option<&str>,
        existing_task_id: Option<&str>,
        actor_runtime: Option<orchestration::allocator::RuntimeRequest>,
        exclude_providers: &[Provider],
    ) -> Result<Arc<orch::Task>, String> {
        let store_dir = self.state.store_dir.clone();
        let is_resume = existing_session_id.is_some();

        // Always use exec-target resolution. The workflow engine owns
        // the project_dir; resume just swaps the provider args call.
        let (
            mut provider,
            lens,
            mut exec_opts,
            mut env_overrides,
            cwd,
            brofile_filters,
            coerce_workspace,
            brofile_context,
        ) = self.resolve_exec_target(Some(brofile), None, project_dir)?;
        let mut runtime_lease = None;
        if is_resume {
            if let Some(lease) = existing_task_id.and_then(|task_id| {
                orchestration::allocator::lookup_lease_for_task(&store_dir, task_id)
            }) {
                provider = lease.provider;
                exec_opts = orchestration::allocator::exec_opts_for_lane(
                    &orchestration::allocator::RuntimeLane {
                        provider,
                        account: lease.account.clone(),
                        tier: lease.tier.clone(),
                        model: lease.model.clone(),
                        effort: lease.effort.clone(),
                        capabilities: lease.capabilities.clone(),
                    },
                );
                // Use the lease-captured brofile context, not whatever the
                // current brofile says — workflow actor brofiles can be
                // edited between turns, and resume must honor the
                // dispatch-time policy.
                env_overrides = orchestration::brofile::resolve_provider_env(
                    provider,
                    lease.account.as_deref(),
                    lease.model.as_deref(),
                    &store_dir,
                    lease.brofile_context.as_ref(),
                );
                runtime_lease = Some(lease);
            }
        }

        if is_resume && !provider.supports_resume() {
            return Err(format!("provider {provider} does not support resume"));
        }

        // Re-enforce against the post-lease runtime provider AND the
        // lease-captured policy. Falls back to the current brofile's
        // context only when there is no lease (fresh dispatch).
        let effective_context = runtime_lease
            .as_ref()
            .and_then(|l| l.brofile_context.as_ref())
            .or(brofile_context.as_ref());
        orchestration::brofile::enforce_provider_defaults(provider, effective_context)?;
        exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            effective_context,
        );

        if !is_resume {
            let brofile_runtime = self
                .resolve_exec_brofile_for_allocator(brofile, project_dir)
                .and_then(|bf| bf.runtime);
            let runtime =
                orchestration::allocator::merge_runtime_request(brofile_runtime, actor_runtime);
            // Safety net mirroring the agent-dispatch path: if the merged
            // request is inert (synthesized only from the durable flag, with
            // no tier/pool/pin), honor the brofile's declared provider rather
            // than letting the allocator free-select across Provider::ALL. A
            // tiered brofile expresses selection intent and is left untouched;
            // a static-provider-only brofile (e.g. a stale untiered actor)
            // pins to its provider instead of landing on a free-select winner.
            let runtime = orchestration::allocator::pin_static_provider_if_inert(
                runtime,
                provider,
                exec_opts.as_ref().and_then(|o| o.model.clone()),
                exec_opts.as_ref().and_then(|o| o.effort.clone()),
            );
            // Cohort-diversity floor: exclude providers already taken by this
            // ensemble's earlier members so a tiered cohort spreads across
            // distinct providers. Empty for single-executor dispatch.
            let runtime = orchestration::allocator::exclude_providers_for_diversity(
                runtime,
                exclude_providers,
            );
            let dispatched =
                self.dispatch_fresh_bro_task(crate::tools::dispatch::FreshDispatchRequest {
                    prompt: prompt.to_string(),
                    provider,
                    lens,
                    exec_opts,
                    env_overrides,
                    cwd,
                    brofile_filters,
                    coerce_workspace,
                    allow_recursion: false,
                    allow_tools: None,
                    disallow_tools: None,
                    allocation_request: runtime,
                    project_dir_for_lease: project_dir.map(String::from),
                    ambient_bro_name: Some(brofile.to_string()),
                    spawn_bro_label: None,
                    spawn_agent_label: None,
                    record_to_bro: Some(brofile.to_string()),
                    brofile_context,
                })?;
            return Ok(dispatched.task);
        }

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = match existing_session_id {
            Some(s) => s.to_string(),
            None if matches!(provider, Provider::Claude) => uuid::Uuid::new_v4().to_string(),
            None => "pending".to_string(),
        };
        let resume_lease = if is_resume {
            match try_acquire_resume_lease(
                &self.state.task_store,
                self.state.resume_leases.as_ref(),
                provider,
                &session_id,
            ) {
                Ok(lease) => Some(lease),
                Err(err) => return Err(err),
            }
        } else {
            None
        };

        let ambient_ctx = orch::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(brofile.to_string()),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(brofile),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: None,
            allow_recursion: false,
            provider: Some(provider),
            coerce_workspace: false,
        };
        let ambient_prompt = orch::apply_ambient(prompt, &ambient_ctx);
        let mut args = if is_resume {
            provider.build_resume_args(&session_id, &ambient_prompt, exec_opts.as_ref())
        } else {
            let final_prompt = orch::apply_brofile_lens(&ambient_prompt, lens.as_deref());
            provider.build_exec_args(
                &final_prompt,
                &session_id,
                cwd.as_deref(),
                exec_opts.as_ref(),
            )
        };

        let dispatch_filters = match resolve_dispatch_filters(
            provider,
            cwd.as_deref(),
            false,
            &task_id,
            brofile_filters.as_ref(),
        ) {
            Ok(df) => df,
            Err(e) => return Err(e),
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
        if let Some(lease) = resume_lease {
            release_resume_lease_when_done(task.clone(), lease);
        }
        self.record_task_to_bro(brofile, &task);
        Ok(task)
    }

    /// Dispatch every member of an ensemble team with the same prompt,
    /// returning one task per member. Each dispatch goes through
    /// `workflow_dispatch_executor`, so durable-session reuse + ambient
    /// context + dispatch filters work uniformly. Unresolved brofiles
    /// are skipped (logged in the returned error string), not fatal.
    pub(crate) async fn workflow_dispatch_ensemble(
        &self,
        team_name: &str,
        prompt: &str,
        project_dir: Option<&str>,
        existing_session_ids: &std::collections::HashMap<String, String>,
        existing_task_ids: &std::collections::HashMap<String, String>,
        actor_runtime: Option<orchestration::allocator::RuntimeRequest>,
    ) -> Result<Vec<(String, Arc<orch::Task>)>, String> {
        // Scope the team lock narrowly — we only need it to read the
        // team's current roster. Holding a parking_lot guard across
        // `.await` makes the resulting future `!Send`, which axum
        // handler bounds reject. Snapshot + drop.
        let (members, project_dir_from_team, diversity_floor): (Vec<_>, _, _) = {
            let _lock = orchestration::team::lock_teams();
            let team = orchestration::team::load_team(team_name, &self.state.store_dir)
                .ok_or_else(|| format!("Unknown team: {team_name}"))?;
            let project_dir_from_team = team.project_dir.clone();
            let diversity_floor = team.diversity_floor.unwrap_or(0);
            let members = team
                .members
                .iter()
                .map(|m| (m.name.clone(), m.brofile.clone()))
                .collect();
            (members, project_dir_from_team, diversity_floor)
        };
        let cwd = project_dir.map(String::from).or(project_dir_from_team);
        let mut launched = Vec::new();
        // Providers resolved by earlier members this dispatch. While we are
        // below the diversity floor we exclude these from the next member's
        // pool so the cohort spreads across distinct providers; once the floor
        // is met (or the pool can't spread further) we stop excluding.
        let mut taken: Vec<Provider> = Vec::new();
        for (member_name, brofile) in &members {
            let existing = existing_session_ids.get(member_name).cloned();
            let existing_task_id = existing_task_ids.get(member_name).cloned();
            let exclude: Vec<Provider> = if diversity_floor > 0 && taken.len() < diversity_floor {
                taken.clone()
            } else {
                Vec::new()
            };
            let task = self
                .workflow_dispatch_executor(
                    brofile,
                    prompt,
                    cwd.as_deref(),
                    existing.as_deref(),
                    existing_task_id.as_deref(),
                    actor_runtime.clone(),
                    &exclude,
                )
                .await
                .map_err(|e| format!("member {member_name}: {e}"))?;
            // Record the lane this member actually resolved to so subsequent
            // members can spread off it.
            let chosen = task.inner.lock().provider;
            if !taken.contains(&chosen) {
                taken.push(chosen);
            }
            // Stamp the precise team::member label, overriding the
            // brofile fallback that workflow_dispatch_executor →
            // record_task_to_bro set. Two team members sharing a
            // brofile (the common keystone-reviewers shape) would
            // otherwise be indistinguishable in `bro tail`.
            task.inner.lock().bro_label = Some(format!("{team_name}::{member_name}"));
            launched.push((member_name.clone(), task));
        }
        Ok(launched)
    }

    /// Apply a workflow-level policy packet to an arc-state entity.
    /// Returns the matching rule's classification (verdict) or `None`.
    pub(crate) fn apply_workflow_policy(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading policy packet {packet_id}: {e:#}"))?;
        let prediction = apply_packet_with(&packet, entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Evaluate a workflow gate packet against a node's output in
    /// mode=first semantics. Returns the matching rule's classification
    /// as the verdict, or `None` when no rule fires. Entity shape is
    /// `{output: <output>, node: <node_id>}` — packet predicates can
    /// reference either field.
    // kept: gate-packet helper for legacy output-only entity shape; engine uses entity-variant now
    #[allow(dead_code)]
    pub(crate) fn apply_workflow_gate(
        &self,
        packet_id: &str,
        output: &str,
        node_id: &str,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let entity = serde_json::json!({
            "output": output,
            "node": node_id,
        });
        let prediction = apply_packet_with(&packet, &entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Evaluate a workflow gate packet in mode=all — every rule whose
    /// antecedent holds emits a finding, the aggregate verdict is the
    /// highest-priority classification in the packet's lattice among
    /// the findings. Returns the verdict + the findings list so the
    /// engine can surface the multi-finding shape in arc notes.
    // kept: gate-packet helper for legacy output-only entity shape; engine uses entity-variant now
    #[allow(dead_code)]
    pub(crate) fn apply_workflow_gate_all(
        &self,
        packet_id: &str,
        output: &str,
        node_id: &str,
    ) -> Result<packets::ApplyAllResult, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let entity = serde_json::json!({
            "output": output,
            "node": node_id,
        });
        Ok(packets::apply_all_with(&packet, &entity, &*packet_store))
    }

    /// Entity-shaped variant of `apply_workflow_gate` — the workflow
    /// engine constructs the full ArcContext flatten (vars + outputs +
    /// meta + last_signal + node_output + node_id) and passes it
    /// directly so packet rules can reference `vars.x`,
    /// `last_signal.name`, etc.
    pub(crate) fn apply_workflow_gate_entity(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
        _node_id: &str,
    ) -> Result<Option<String>, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        let prediction = apply_packet_with(&packet, entity, &*packet_store);
        Ok(prediction.map(|p| p.classification))
    }

    /// Entity-shaped `apply_all` variant. Same shape as
    /// `apply_workflow_gate_entity` but mode=all semantics.
    pub(crate) fn apply_workflow_gate_all_entity(
        &self,
        packet_id: &str,
        entity: &serde_json::Value,
        _node_id: &str,
    ) -> Result<packets::ApplyAllResult, String> {
        let packet_store = self.state.packets.read();
        let packet = packet_store
            .load(packet_id)
            .map_err(|e| format!("loading gate packet {packet_id}: {e:#}"))?;
        Ok(packets::apply_all_with(&packet, entity, &*packet_store))
    }

    /// Server-owned WaitStore for suspendable arcs.
    pub(crate) fn wait_store(&self) -> &Arc<crate::workflow::wait::WaitStore> {
        &self.state.wait_store
    }

    /// Register an arc cancel token. Called by the workflow runner at
    /// startup; returned token is stored on the runner and observed
    /// between node iterations and inside Wait suspensions.
    pub(crate) fn register_arc_cancel_token(&self, arc_id: &str) -> CancellationToken {
        self.state.register_arc_cancel_token(arc_id)
    }

    /// Register an arc cancel token chained to a parent arc/group
    /// token. Used by nested workflows so cancellation propagates
    /// down the composition tree.
    pub(crate) fn register_arc_cancel_token_child(
        &self,
        arc_id: &str,
        parent: &CancellationToken,
    ) -> CancellationToken {
        self.state.register_arc_cancel_token_child(arc_id, parent)
    }

    /// Drop the arc's cancel token. Called by the runner at terminus.
    pub(crate) fn unregister_arc_cancel_token(&self, arc_id: &str) {
        self.state.unregister_arc_cancel_token(arc_id);
    }

    /// Resolve a workflow by registry id (set via `bro_workflow_install`
    /// or restored from disk on startup). Returns a clone so the caller
    /// can mutate locally without affecting the registry.
    pub(crate) fn resolve_workflow_by_id(&self, id: &str) -> Option<workflow::Workflow> {
        self.state.workflow_registry.read().get(id).cloned()
    }
}

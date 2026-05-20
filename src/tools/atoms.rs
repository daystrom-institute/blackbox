mod composition;
mod helpers;
mod invoke;
mod supervision;
use crate::orchestration;
use crate::server::BlackboxServer;
use crate::tools::bro_params::{
    AtomDelegateParams, AtomDescribeParams, AtomGetParams, AtomInvokeParams, AtomListParams,
    AtomResumeParams, AtomSearchParams, AtomStatusParams,
};
#[cfg(test)]
use helpers::atom_ref_allowed;
use helpers::{default_atom_owner, iso_from_millis, sha256_text};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::CallToolResult;
use rmcp::{tool, tool_router};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::atoms_tools()
}

#[tool_router(router = atoms_tools)]
impl BlackboxServer {
    #[tool(
        name = "atom_list",
        description = "List installed atoms from the registry. Optional filters for cost_class, provenance_kind, subcontract, include_superseded, and limit."
    )]
    pub(crate) fn atom_list(&self, Parameters(p): Parameters<AtomListParams>) -> CallToolResult {
        use orchestration::atoms::registry::{AtomListFilter, AtomRegistry};
        use orchestration::atoms::types::AtomCostClass;
        let catalog = self.state.artifacts.read();
        let reg = AtomRegistry::new(&catalog);
        let cost_class = match p.cost_class.as_deref() {
            Some(s) => {
                let parsed: AtomCostClass = match serde_json::from_value(serde_json::Value::String(
                    s.to_string(),
                )) {
                    Ok(c) => c,
                    Err(_) => {
                        return Self::err_text(&format!(
                            "unknown cost_class: {s} (expected one of: cheap, normal, expensive)"
                        ));
                    }
                };
                Some(parsed)
            }
            None => None,
        };
        let filter = AtomListFilter {
            include_superseded: p.include_superseded.unwrap_or(false),
            cost_class,
            provenance_kind: p.provenance_kind,
            subcontract: p.subcontract,
        };
        match reg.list(&filter) {
            Ok(summaries) => {
                let capped = match p.limit {
                    Some(n) => summaries.into_iter().take(n).collect::<Vec<_>>(),
                    None => summaries,
                };
                Self::ok_json(&serde_json::json!({
                    "atoms": capped.iter().map(|s| {
                        let mut m = serde_json::Map::from_iter([
                            ("name".into(), serde_json::Value::String(s.name.clone())),
                            ("version".into(), serde_json::Value::String(s.version.clone())),
                            ("active".into(), serde_json::Value::Bool(s.active)),
                            ("installed_at".into(), serde_json::Value::String(s.installed_at.clone())),
                        ]);
                        if let Some(desc) = &s.description {
                            m.insert("description".into(), serde_json::Value::String(desc.clone()));
                        }
                        if let Some(cc) = &s.cost_class {
                            m.insert("cost_class".into(), serde_json::Value::String(cc.to_string()));
                        }
                        if let Some(pk) = &s.provenance_kind {
                            m.insert("provenance_kind".into(), serde_json::Value::String(pk.clone()));
                        }
                        if let Some(sc) = &s.subcontract {
                            m.insert("subcontract".into(), serde_json::Value::String(sc.clone()));
                        }
                        if let Some(ik) = &s.implementation_kind {
                            m.insert("implementation_kind".into(), serde_json::Value::String(ik.clone()));
                        }
                        if !s.supersedes_chain.is_empty() {
                            m.insert(
                                "supersedes_chain".into(),
                                serde_json::Value::Array(
                                    s.supersedes_chain.iter().map(|c| serde_json::Value::String(c.clone())).collect(),
                                ),
                            );
                        }
                        serde_json::Value::Object(m)
                    }).collect::<Vec<_>>()
                }))
            }
            Err(e) => Self::err_text(&format!("atom registry list failed: {e}")),
        }
    }

    #[tool(
        name = "atom_get",
        description = "Read full details for a single atom by name or atom-ref (atom:name@vN, atom:name@latest, or bare name). Returns manifest, metadata, lifecycle state, and subcontract."
    )]
    pub(crate) fn atom_get(&self, Parameters(p): Parameters<AtomGetParams>) -> CallToolResult {
        use orchestration::atoms::registry::AtomRegistry;
        let catalog = self.state.artifacts.read();
        let reg = AtomRegistry::new(&catalog);
        match reg.get(&p.name) {
            Ok(Some(rec)) => {
                let mut m = serde_json::Map::from_iter([
                    ("name".into(), serde_json::Value::String(rec.name)),
                    ("version".into(), serde_json::Value::String(rec.version)),
                    ("active".into(), serde_json::Value::Bool(rec.active)),
                    (
                        "installed_at".into(),
                        serde_json::Value::String(rec.installed_at),
                    ),
                    ("source".into(), serde_json::Value::String(rec.source)),
                ]);
                if let Some(s) = rec.metadata.supersedes {
                    m.insert("supersedes".into(), serde_json::Value::String(s));
                }
                if !rec.metadata.supersedes_chain.is_empty() {
                    m.insert(
                        "supersedes_chain".into(),
                        serde_json::Value::Array(
                            rec.metadata
                                .supersedes_chain
                                .into_iter()
                                .map(serde_json::Value::String)
                                .collect(),
                        ),
                    );
                }
                if let Some(s) = rec.metadata.superseded_by {
                    m.insert("superseded_by".into(), serde_json::Value::String(s));
                }
                if let Some(parse_err) = rec.manifest_parse_error {
                    m.insert(
                        "manifest_parse_error".into(),
                        serde_json::Value::String(parse_err),
                    );
                }
                if let Some(manifest) = rec.manifest {
                    m.insert(
                        "manifest".into(),
                        serde_json::to_value(manifest).unwrap_or_else(|e| {
                            serde_json::Value::String(format!("<serialize error: {e}>"))
                        }),
                    );
                }
                Self::ok_json(&serde_json::Value::Object(m))
            }
            Ok(None) => Self::err_text(&format!("atom not found: {}", p.name)),
            Err(e) => Self::err_text(&format!("atom registry get failed: {e}")),
        }
    }

    #[tool(
        name = "atom_describe",
        description = "Full manifest + implementation details for one atom. Returns the complete manifest including effects, composition constraints, supervision policy, and any install warnings."
    )]
    pub(crate) fn atom_describe(
        &self,
        Parameters(p): Parameters<AtomDescribeParams>,
    ) -> CallToolResult {
        use orchestration::atoms::registry::AtomRegistry;
        let catalog = self.state.artifacts.read();
        let reg = AtomRegistry::new(&catalog);
        let rec = match reg.get(&p.atom) {
            Ok(Some(r)) => r,
            Ok(None) => return Self::err_text(&format!("atom not found: {}", p.atom)),
            Err(e) => return Self::err_text(&format!("atom registry get failed: {e}")),
        };
        let manifest = match rec.manifest {
            Some(m) => m,
            None => {
                return Self::ok_json(&serde_json::json!({
                    "name": rec.name,
                    "version": rec.version,
                    "active": rec.active,
                    "error": format!("manifest parse failed: {}", rec.manifest_parse_error.unwrap_or_default()),
                }));
            }
        };

        let install_warnings = rec
            .metadata
            .install_warnings
            .iter()
            .cloned()
            .map(serde_json::Value::String)
            .collect::<Vec<_>>();

        let mut result = serde_json::Map::from_iter([
            ("name".into(), serde_json::Value::String(rec.name)),
            ("version".into(), serde_json::Value::String(rec.version)),
            ("active".into(), serde_json::Value::Bool(rec.active)),
            (
                "implementation_kind".into(),
                serde_json::Value::String(match &manifest.implementation {
                    orchestration::atoms::types::AtomImplementation::Profile { .. } => {
                        "profile".to_string()
                    }
                    orchestration::atoms::types::AtomImplementation::Workflow { .. } => {
                        "workflow".to_string()
                    }
                    orchestration::atoms::types::AtomImplementation::Deterministic { .. } => {
                        "deterministic".to_string()
                    }
                    orchestration::atoms::types::AtomImplementation::Adapter { .. } => {
                        "adapter".to_string()
                    }
                }),
            ),
            (
                "install_warnings".into(),
                serde_json::Value::Array(install_warnings),
            ),
        ]);
        result.insert(
            "manifest".into(),
            serde_json::to_value(&manifest).unwrap_or(serde_json::Value::Null),
        );
        Self::ok_json(&serde_json::Value::Object(result))
    }

    #[tool(
        name = "atom_search",
        description = "Search installed atoms by query string. Matches against description and when_to_use; penalizes or excludes results matching anti_patterns. Returns ranked results with scores and provenance."
    )]
    pub(crate) fn atom_search(
        &self,
        Parameters(p): Parameters<AtomSearchParams>,
    ) -> CallToolResult {
        use orchestration::atoms::registry::{AtomRegistry, AtomSearchFilter};
        use orchestration::atoms::types::AtomCostClass;
        let query = p.query.trim();
        if query.is_empty() {
            return Self::err_text("query is required");
        }
        let limit = p.limit.unwrap_or(5).min(50) as usize;
        let cost_class = match p.cost_class.as_deref() {
            Some("cheap") => Some(AtomCostClass::Cheap),
            Some("normal") => Some(AtomCostClass::Normal),
            Some("expensive") => Some(AtomCostClass::Expensive),
            Some(other) => return Self::err_text(&format!("invalid cost_class: {other}")),
            None => None,
        };
        let filter = AtomSearchFilter {
            cost_class,
            provenance_kind: p.provenance_kind,
            subcontract: p.subcontract,
        };
        let exclude_ap = p.exclude_anti_pattern_matches.unwrap_or(true);
        let catalog = self.state.artifacts.read();
        let reg = AtomRegistry::new(&catalog);
        let results = match reg.search(query, limit, &filter, exclude_ap) {
            Ok(r) => r,
            Err(e) => return Self::err_text(&format!("atom search failed: {e}")),
        };
        let json_results: Vec<serde_json::Value> = results
            .iter()
            .map(|r| {
                let mut obj = serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "score": (r.score * 1000.0).round() / 1000.0,
                    "description": r.description,
                    "when_to_use": r.when_to_use,
                    "anti_patterns": r.anti_patterns,
                    "cost_class": r.cost_class.map(|c| c.to_string()),
                    "provenance_kind": r.provenance_kind,
                    "sources": r.sources,
                });
                if let Some(sc) = &r.subcontract {
                    obj["subcontract"] = serde_json::Value::String(sc.clone());
                }
                if !exclude_ap {
                    obj["matched_anti_patterns"] = serde_json::json!(r.matched_anti_patterns);
                }
                obj
            })
            .collect();
        Self::ok_json(&serde_json::json!({
            "results": json_results,
            "total_matched": json_results.len(),
        }))
    }

    // ── atom_invoke ─────────────────────────────────────────────

    #[tool(
        name = "atom_invoke",
        description = "Invoke an installed atom. Resolves the atom manifest, validates policy gates (effects, composition, depth), and dispatches via the appropriate implementation path (profile, workflow, deterministic, adapter). Returns an owned invocation handle with invocation_id and underlying task/session ids."
    )]
    pub(crate) async fn atom_invoke(
        &self,
        Parameters(p): Parameters<AtomInvokeParams>,
    ) -> CallToolResult {
        match self.atom_invoke_value(p, None).await {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::err_text(&e),
        }
    }

    // ── atom_status ─────────────────────────────────────────────

    fn refresh_atom_invocation_from_task(
        &self,
        inv: &mut orchestration::atoms::invocation::AtomInvocation,
    ) {
        use orchestration::atoms::invocation::{AtomHandle, InvocationStatus};

        let (task_id, session_id_slot) = match &mut inv.handle {
            AtomHandle::Profile {
                task_id,
                session_id,
                ..
            } => (task_id.clone(), Some(session_id)),
            AtomHandle::Workflow {
                root_task_id: Some(task_id),
                ..
            } => (task_id.clone(), None),
            _ => return,
        };

        let task_store = self.state.task_store.read();
        let Some(task) = task_store.get(&task_id) else {
            return;
        };
        let inner = task.inner.lock();
        inv.status = match inner.status {
            orchestration::TaskStatus::Completed => InvocationStatus::Succeeded,
            orchestration::TaskStatus::Failed => InvocationStatus::Failed,
            orchestration::TaskStatus::Running => InvocationStatus::Running,
            orchestration::TaskStatus::Cancelled => InvocationStatus::Cancelled,
        };
        if let Some(session_id) = session_id_slot
            && *session_id == "pending"
            && inner.session_id != "pending"
        {
            *session_id = inner.session_id.clone();
        }
        if let Some(usage) = &inner.usage {
            inv.cost.input_tokens = Some(usage.input_tokens);
            inv.cost.output_tokens = Some(usage.output_tokens);
        }
        if let Some(completed_at) = inner.completed_at {
            inv.ended_at = Some(iso_from_millis(completed_at));
        }
        let end = inner.completed_at.unwrap_or_else(orchestration::now_ms);
        inv.cost.wall_time_ms = Some(end.saturating_sub(inner.started_at));
        if let Some(msg) = &inner.last_assistant_message {
            if inv.structured_output.is_none()
                && let Ok(value) = serde_json::from_str::<serde_json::Value>(msg)
                && let Some(structured) = value.get("structured_exit")
                && !structured.is_null()
            {
                inv.structured_output = Some(structured.clone());
            }
            if inv.summary.is_none() {
                inv.summary = Some(msg.chars().take(500).collect());
            }
            if inv.output_digest.is_none() {
                inv.output_digest = Some(sha256_text(msg));
            }
        }
    }

    #[tool(
        name = "atom_status",
        description = "Read the status of an atom invocation. Ownership-gated: only owners can read status. Returns a normalized trace envelope with state, timestamps, effects observed, cost, and summary."
    )]
    pub(crate) fn atom_status(
        &self,
        Parameters(p): Parameters<AtomStatusParams>,
    ) -> CallToolResult {
        match self.atom_status_value(p) {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::err_text(&e),
        }
    }

    pub(crate) fn atom_status_value(
        &self,
        p: AtomStatusParams,
    ) -> Result<serde_json::Value, String> {
        let mut inv = {
            let inv_store = self.state.atom_invocation_store.read();
            match inv_store.get(&p.invocation_id).cloned() {
                Some(i) => i,
                None => {
                    return Err(format!("invocation not found: {}", p.invocation_id));
                }
            }
        };
        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let caller = owner.as_str();
        if !inv.is_owner(caller) {
            return Err("error.forbidden: caller is not an owner of this invocation".into());
        }

        {
            self.refresh_atom_invocation_from_task(&mut inv);
            self.state.atom_invocation_store.write().update(inv.clone());
        }

        Ok(inv.to_trace_envelope())
    }

    // ── atom_resume ─────────────────────────────────────────────

    #[tool(
        name = "atom_resume",
        description = "Resume a profile-backed atom invocation. Ownership-gated and only for resumable handles (profile-backed, in a runnable state). Resumes underlying provider session using existing bro resume internals."
    )]
    pub(crate) async fn atom_resume(
        &self,
        Parameters(p): Parameters<AtomResumeParams>,
    ) -> CallToolResult {
        match self.atom_resume_value(p).await {
            Ok(value) => Self::ok_json(&value),
            Err(e) => Self::err_text(&e),
        }
    }

    pub(crate) async fn atom_resume_value(
        &self,
        p: AtomResumeParams,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::{AtomHandle, InvocationStatus};
        use orchestration::providers::ExecOpts;

        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let caller = owner.as_str();
        let mut inv = {
            let inv_store = self.state.atom_invocation_store.read();
            match inv_store.get(&p.invocation_id).cloned() {
                Some(i) => i,
                None => {
                    return Err(format!("invocation not found: {}", p.invocation_id));
                }
            }
        };
        if !inv.is_owner(caller) {
            return Err("error.forbidden: caller is not an owner of this invocation".into());
        }
        self.refresh_atom_invocation_from_task(&mut inv);
        if !inv.is_resumable() {
            return Err(
                "error.not_resumable: this invocation handle does not support resume (deterministic/adapter/workflow handles are not resumable, or invocation is in a terminal non-runnable state)"
                    .into(),
            );
        }

        let (session_id, provider_str, cwd, handle_task_id) = match &inv.handle {
            AtomHandle::Profile {
                session_id,
                provider,
                project_dir,
                task_id,
                ..
            } => (
                session_id.clone(),
                provider.clone(),
                project_dir.clone(),
                task_id.clone(),
            ),
            _ => unreachable!("is_resumable checked above"),
        };
        if session_id == "pending" {
            return Err(
                "error.not_ready(code=session_pending): provider has not emitted a resumable session id yet; call atom_status again later"
                    .into(),
            );
        }

        let mut provider = match provider_str.parse::<orchestration::providers::Provider>() {
            Ok(p) => p,
            Err(_) => return Err(format!("invalid provider: {provider_str}")),
        };

        let (_, _, manifest) = match self.resolve_active_atom_manifest(&inv.atom_ref) {
            Ok(found) => found,
            Err(e) => return Err(e),
        };
        let brofile_ref = match &manifest.implementation {
            orchestration::atoms::types::AtomImplementation::Profile { brofile_ref } => brofile_ref,
            _ => {
                return Err(
                    "error.not_resumable: only profile-backed atom handles can resume through a provider session"
                        .into(),
                );
            }
        };
        let brofile_name =
            match orchestration::atoms::validate::parse_typed_ref(brofile_ref, "brofile:") {
                Ok((name, _ver)) => name,
                Err(e) => return Err(format!("invalid brofile_ref: {e}")),
            };
        let bf = match orchestration::brofile::resolve_brofile(
            &brofile_name,
            &self.state.store_dir,
            cwd.as_deref(),
        ) {
            Some(b) => b,
            None => return Err(format!("brofile '{}' not found", brofile_name)),
        };
        let selected_lease =
            orchestration::allocator::lookup_lease_for_task(&self.state.store_dir, &handle_task_id);
        if let Some(lease) = &selected_lease {
            provider = lease.provider;
        } else if bf.provider != provider {
            return Err(format!(
                "error.not_resumable(code=provider_changed): atom brofile now resolves to provider {}, but handle was created with {}",
                bf.provider.as_str(),
                provider.as_str()
            ));
        }
        if !provider.supports_resume() {
            return Err(format!(
                "provider '{}' does not support resume",
                provider.as_str()
            ));
        }
        // Enforce against the post-lease runtime provider AND the
        // lease-captured brofile context, not the current `bf.context`.
        // The brofile may have been edited between dispatch and resume —
        // resume must honor the policy the session was launched under,
        // not whatever the brofile says today.
        let effective_context = selected_lease
            .as_ref()
            .and_then(|l| l.brofile_context.as_ref())
            .or(bf.context.as_ref());
        orchestration::brofile::enforce_provider_defaults(provider, effective_context)?;
        let exec_opts = if let Some(lease) = &selected_lease {
            orchestration::allocator::exec_opts_for_lane(&orchestration::allocator::RuntimeLane {
                provider,
                account: lease.account.clone(),
                tier: lease.tier.clone(),
                model: lease.model.clone(),
                effort: lease.effort.clone(),
                capabilities: lease.capabilities.clone(),
            })
        } else if bf.model.is_some() || bf.effort.is_some() {
            Some(ExecOpts {
                model: bf.model.clone(),
                effort: bf.effort.clone(),
                provider_defaults: None,
            })
        } else {
            None
        };
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            effective_context,
        );
        let (env_account, env_model) = if let Some(lease) = &selected_lease {
            (lease.account.as_deref(), lease.model.as_deref())
        } else {
            (bf.account.as_deref(), bf.model.as_deref())
        };
        let env_overrides = orchestration::brofile::resolve_provider_env(
            provider,
            env_account,
            env_model,
            &self.state.store_dir,
            effective_context,
        );

        let resume_cwd: Option<String> = provider
            .resolve_session_cwd(&session_id)
            .map(|p| p.to_string_lossy().to_string())
            .or_else(|| cwd.clone());
        let task_id_new = uuid::Uuid::new_v4().to_string();
        let resume_lease = match crate::server::progress::try_acquire_resume_lease(
            &self.state.task_store,
            self.state.resume_leases.as_ref(),
            provider,
            &session_id,
        ) {
            Ok(lease) => lease,
            Err(e) => return Err(e),
        };

        let ambient_ctx = orchestration::AmbientContext {
            task_id: Some(task_id_new.clone()),
            session_id: Some(session_id.clone()),
            project_dir: resume_cwd.clone(),
            bro_name: Some(inv.atom_ref.clone()),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                resume_cwd.as_deref(),
                Some(inv.atom_ref.as_str()),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: Some(orchestration::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(provider),
            coerce_workspace: bf.coerce_workspace.unwrap_or(false),
        };
        let wrapped_prompt = orchestration::apply_ambient(&p.prompt, &ambient_ctx);

        let mut args = provider.build_resume_args(&session_id, &wrapped_prompt, exec_opts.as_ref());
        let brofile_filters = bf.filters.clone();
        let dispatch_filters = match crate::server::progress::resolve_dispatch_filters(
            provider,
            resume_cwd.as_deref(),
            false,
            &task_id_new,
            brofile_filters.as_ref(),
            None,
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Err(format!("dispatch filter resolution failed: {e}")),
        };
        args.extend(dispatch_filters.args);

        let task = orchestration::spawn_task(
            task_id_new.clone(),
            provider,
            args,
            session_id.clone(),
            resume_cwd.clone(),
            env_overrides,
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(inv.atom_ref.clone()),
            Some(inv.atom_ref.clone()),
            Some(self.state.system_events.clone()),
        );
        if let Some(lease) = &selected_lease {
            let inner = task.inner.lock();
            orchestration::allocator::record_lease(
                &self.state.store_dir,
                orchestration::allocator::lease_for_resume_task(
                    lease,
                    inner.id.clone(),
                    inner.session_id.clone(),
                    inner.cwd.clone(),
                ),
            );
        }

        crate::server::progress::cleanup_policy_file_when_done(
            task.clone(),
            dispatch_filters.policy_file,
        );
        crate::server::progress::release_resume_lease_when_done(task.clone(), resume_lease);

        {
            let mut inv_store = self.state.atom_invocation_store.write();
            if let Some(inv) = inv_store.get_mut(&p.invocation_id) {
                inv.status = InvocationStatus::Running;
                inv.ended_at = None;
                inv.output_digest = None;
                if let AtomHandle::Profile {
                    task_id,
                    session_id: handle_session_id,
                    project_dir,
                    ..
                } = &mut inv.handle
                {
                    *task_id = task_id_new.clone();
                    *handle_session_id = session_id.clone();
                    *project_dir = resume_cwd.clone();
                }
            }
            let _ = inv_store.persist();
        }

        Ok(serde_json::json!({
            "invocation_id": p.invocation_id,
            "task_id": task_id_new,
            "session_id": session_id,
            "status": "running",
        }))
    }

    // ── atom_delegate ───────────────────────────────────────────

    #[tool(
        name = "atom_delegate",
        description = "Grant another owner access to an atom invocation. Owner-only. v1 does not support revocation. Delegated owners gain full status/resume/delegate rights."
    )]
    pub(crate) fn atom_delegate(
        &self,
        Parameters(p): Parameters<AtomDelegateParams>,
    ) -> CallToolResult {
        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let caller = owner.as_str();
        {
            let inv_store = self.state.atom_invocation_store.read();
            let inv = match inv_store.get(&p.invocation_id) {
                Some(i) => i,
                None => {
                    return Self::err_text(&format!("invocation not found: {}", p.invocation_id));
                }
            };
            if !inv.is_owner(caller) {
                return Self::err_text("error.forbidden: only existing owners can delegate");
            }
        }
        {
            let mut inv_store = self.state.atom_invocation_store.write();
            if let Some(inv) = inv_store.get_mut(&p.invocation_id) {
                inv.add_owner(&p.grant_to);
            }
            let _ = inv_store.persist();
        }
        Self::ok_json(&serde_json::json!({
            "invocation_id": p.invocation_id,
            "granted_to": p.grant_to,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::{self, ArtifactInstallParams};
    use crate::server::install_artifact_value;
    use crate::server::state::SharedState;
    use crate::server::workflow_capabilities::validate_workflow_capabilities;
    use crate::tools::atoms::helpers::bounded_effect_u64;
    use crate::workflow;
    use std::sync::Arc;

    fn test_server(tmp: &tempfile::TempDir) -> BlackboxServer {
        BlackboxServer::new(Arc::new(SharedState::for_test(&tmp.path().join("bro"))))
    }

    fn extract_text(result: &CallToolResult) -> String {
        let wire = serde_json::to_value(result).unwrap();
        wire["content"][0]["text"].as_str().unwrap().to_string()
    }

    fn make_task(
        server: &BlackboxServer,
        task_id: &str,
        events: Vec<serde_json::Value>,
        last_message: Option<&str>,
        report: Option<orchestration::BroReport>,
    ) -> Arc<orchestration::Task> {
        let task = orchestration::spawn_in_process_task(
            task_id.to_string(),
            crate::orchestration::providers::Provider::Codex,
            "session-primary".to_string(),
            None,
            server.state.store_dir.clone(),
            server.state.task_store.clone(),
            server.state.tail_tx.clone(),
            None,
            None,
            None,
        );
        {
            let mut inner = task.inner.lock();
            inner.events = events;
            inner.last_assistant_message = last_message.map(str::to_string);
            inner.report = report;
        }
        task
    }

    #[test]
    fn atom_ref_allowed_accepts_exact_and_latest_refs() {
        assert!(atom_ref_allowed(
            &["atom:rust-review@v1".to_string()],
            "atom:rust-review@v1"
        ));
        assert!(atom_ref_allowed(
            &["atom:rust-review@latest".to_string()],
            "atom:rust-review@v7"
        ));
        assert!(!atom_ref_allowed(
            &["atom:rust-review@v1".to_string()],
            "atom:rust-review@v2"
        ));
    }

    #[test]
    fn bounded_effect_u64_parses_bounded_and_unbounded() {
        assert_eq!(
            bounded_effect_u64(Some(&serde_json::json!(3))).unwrap(),
            Some(3)
        );
        assert_eq!(
            bounded_effect_u64(Some(&serde_json::json!("unbounded"))).unwrap(),
            None
        );
        assert!(bounded_effect_u64(Some(&serde_json::json!(-1))).is_err());
    }

    #[test]
    fn default_atom_owner_is_stable_for_omitted_owner_tools() {
        assert_eq!(default_atom_owner(), "operator:local");
    }

    #[test]
    fn attached_supervision_poll_value_authorizes_lineage_and_denies_unrelated() {
        let dir = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
            dir.path(),
        )));

        let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-primary".into(),
            "atom:test@v1".into(),
            None,
            "operator:primary".into(),
            "claude".into(),
            "session-primary".into(),
            None,
            "task-primary".into(),
        );
        let classifier = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-classifier".into(),
            "atom:test@v1".into(),
            None,
            "operator:classifier".into(),
            "claude".into(),
            "session-classifier".into(),
            None,
            "task-classifier".into(),
        );

        {
            let mut store = server.state.atom_invocation_store.write();
            store.insert(primary);
            store.insert(classifier);
            store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
                supervision_run_id: "run-1".into(),
                primary_invocation_id: "inv-primary".into(),
                primary_task_id: "task-primary".into(),
                classifier_invocation_id: Some("inv-classifier".into()),
                advisor_invocation_id: None,
                attempt: 1,
            });
        }

        make_task(&server, "task-primary", vec![], Some("still running"), None);

        assert!(
            server
                .attached_supervision_poll_value("inv-primary", "operator:classifier", Some(1))
                .is_ok()
        );
        let missing_attempt =
            server.attached_supervision_poll_value("inv-primary", "operator:classifier", Some(2));
        assert!(missing_attempt.is_err());
        let denied =
            server.attached_supervision_poll_value("inv-primary", "operator:stranger", Some(1));
        assert!(denied.is_err());
    }

    #[test]
    fn attached_supervision_poll_value_bounds_note_and_tail_sizes() {
        let dir = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
            dir.path(),
        )));

        let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-primary".into(),
            "atom:test@v1".into(),
            None,
            "operator:primary".into(),
            "claude".into(),
            "session-primary".into(),
            None,
            "task-primary".into(),
        );

        {
            let mut store = server.state.atom_invocation_store.write();
            store.insert(primary);
            store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
                supervision_run_id: "run-1".into(),
                primary_invocation_id: "inv-primary".into(),
                primary_task_id: "task-primary".into(),
                classifier_invocation_id: None,
                advisor_invocation_id: None,
                attempt: 1,
            });
        }

        let mut events = Vec::new();
        for i in 0..40 {
            events.push(serde_json::json!({ "i": i }));
        }
        make_task(
            &server,
            "task-primary",
            events,
            Some(&"x".repeat(5000)),
            None,
        );

        for i in 0..25 {
            let _ = server
                .state
                .notes
                .write()
                .create(&crate::notes::NoteParams {
                    kind: "assumption".into(),
                    body: format!("note-{} {}", i, "y".repeat(5000)),
                    task_id: Some("task-primary".into()),
                    session_id: None,
                    project: None,
                    thread_id: None,
                    provider: None,
                    bro: None,
                })
                .unwrap();
        }

        let snapshot = server
            .attached_supervision_poll_value("inv-primary", "operator:primary", Some(1))
            .unwrap();
        assert_eq!(
            snapshot["attempt_metadata"]["attempt"],
            serde_json::json!(1)
        );
        assert_eq!(
            snapshot["recent_provider_events"].as_array().unwrap().len(),
            20
        );
        assert_eq!(snapshot["task_notes"].as_array().unwrap().len(), 20);
        assert!(snapshot["assistant_tail"].as_str().unwrap().len() <= 4000);
        for note in snapshot["task_notes"].as_array().unwrap() {
            assert!(note["body"].as_str().unwrap().len() <= 4000);
        }
    }

    #[tokio::test]
    async fn execute_supervision_action_accept_records_without_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
            dir.path(),
        )));
        let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-primary".into(),
            "atom:test@v1".into(),
            None,
            "operator:primary".into(),
            "claude".into(),
            "session-primary".into(),
            None,
            "task-primary".into(),
        );
        let advisor = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-advisor".into(),
            "atom:advisor@v1".into(),
            None,
            "operator:advisor".into(),
            "claude".into(),
            "session-advisor".into(),
            None,
            "task-advisor".into(),
        );
        {
            let mut store = server.state.atom_invocation_store.write();
            store.insert(primary);
            store.insert(advisor);
            store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
                supervision_run_id: "run-1".into(),
                primary_invocation_id: "inv-primary".into(),
                primary_task_id: "task-primary".into(),
                classifier_invocation_id: None,
                advisor_invocation_id: Some("inv-advisor".into()),
                attempt: 1,
            });
        }
        make_task(&server, "task-primary", vec![], Some("done"), None);

        let result = server
            .execute_supervision_action_value(
                "inv-primary",
                "operator:advisor",
                Some(1),
                serde_json::json!({"action": "accept", "reason": "meets criteria"}),
            )
            .await
            .unwrap();
        assert_eq!(result["result"]["status"], serde_json::json!("recorded"));
        assert_eq!(
            result["result"]["mutated_primary"],
            serde_json::json!(false)
        );
    }

    #[tokio::test]
    async fn execute_supervision_action_cancel_scopes_to_primary_task() {
        let dir = tempfile::tempdir().unwrap();
        let server = BlackboxServer::new(Arc::new(crate::server::state::SharedState::for_test(
            dir.path(),
        )));
        let primary = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-primary".into(),
            "atom:test@v1".into(),
            None,
            "operator:primary".into(),
            "claude".into(),
            "session-primary".into(),
            None,
            "task-primary".into(),
        );
        let advisor = orchestration::atoms::invocation::AtomInvocation::new_profile(
            "inv-advisor".into(),
            "atom:advisor@v1".into(),
            None,
            "operator:advisor".into(),
            "claude".into(),
            "session-advisor".into(),
            None,
            "task-advisor".into(),
        );
        {
            let mut store = server.state.atom_invocation_store.write();
            store.insert(primary);
            store.insert(advisor);
            store.insert_attachment(orchestration::atoms::invocation::SupervisionAttachment {
                supervision_run_id: "run-1".into(),
                primary_invocation_id: "inv-primary".into(),
                primary_task_id: "task-primary".into(),
                classifier_invocation_id: None,
                advisor_invocation_id: Some("inv-advisor".into()),
                attempt: 1,
            });
        }
        make_task(&server, "task-primary", vec![], Some("running"), None);
        make_task(&server, "task-unrelated", vec![], Some("running"), None);

        let result = server
            .execute_supervision_action_value(
                "inv-primary",
                "operator:advisor",
                Some(1),
                serde_json::json!({"action": "cancel_and_retry", "reason": "retry"}),
            )
            .await
            .unwrap();
        assert_eq!(result["result"]["status"], serde_json::json!("cancelled"));

        let primary_task = server.state.task_store.read().get("task-primary").unwrap();
        assert!(matches!(
            primary_task.inner.lock().status,
            orchestration::TaskStatus::Cancelled
        ));
        let unrelated_task = server
            .state
            .task_store
            .read()
            .get("task-unrelated")
            .unwrap();
        assert!(matches!(
            unrelated_task.inner.lock().status,
            orchestration::TaskStatus::Running
        ));
    }
    fn deterministic_echo_atom(name: &str) -> serde_json::Value {
        serde_json::json!({
            "_contract": "atom/v1",
            "kind": "atom",
            "name": name,
            "version": 1,
            "manifest": {
                "description": "Echo deterministic atom for runtime tests.",
                "when_to_use": ["when testing deterministic atom invocation"],
                "inputs": {
                    "schema": {
                        "type": "object",
                        "additionalProperties": true
                    }
                },
                "outputs": {
                    "schema": {
                        "type": "object",
                        "required": ["echo"],
                        "properties": {
                            "echo": {}
                        }
                    }
                },
                "effects": {
                    "writes_files": false,
                    "dispatches_runs": 0,
                    "max_depth": 0,
                    "uses_network": false
                },
                "composition": {
                    "may_invoke_atoms": {"kind": "none"}
                },
                "implementation": {
                    "kind": "deterministic",
                    "runner": "echo"
                }
            }
        })
    }

    fn badgey_adapter_atom(name: &str) -> serde_json::Value {
        serde_json::json!({
            "_contract": "atom/v1",
            "kind": "atom",
            "name": name,
            "version": 1,
            "manifest": {
                "description": "Badgey adapter atom for runtime tests.",
                "when_to_use": ["when testing adapter atom invocation"],
                "inputs": {
                    "schema": {"type": "object", "additionalProperties": true}
                },
                "outputs": {
                    "schema": {
                        "type": "object",
                        "required": ["adapter", "accepted"],
                        "properties": {
                            "adapter": {"const": "badgey"},
                            "accepted": {"const": true}
                        }
                    }
                },
                "effects": {
                    "writes_files": false,
                    "dispatches_runs": 0,
                    "max_depth": 0,
                    "uses_network": false
                },
                "composition": {
                    "may_invoke_atoms": {"kind": "none"}
                },
                "implementation": {
                    "kind": "adapter",
                    "adapter_name": "badgey"
                }
            }
        })
    }

    fn workflow_wrapper_atom(name: &str, workflow_ref: &str) -> serde_json::Value {
        serde_json::json!({
            "_contract": "atom/v1",
            "kind": "atom",
            "name": name,
            "version": 1,
            "manifest": {
                "description": "Workflow-backed atom for runtime tests.",
                "when_to_use": ["when testing workflow atom invocation"],
                "inputs": {
                    "schema": {"type": "object", "additionalProperties": true}
                },
                "effects": {
                    "writes_files": false,
                    "dispatches_runs": 1,
                    "max_depth": 0,
                    "uses_network": false
                },
                "composition": {
                    "may_invoke_atoms": {"kind": "none"}
                },
                "implementation": {
                    "kind": "workflow",
                    "workflow_ref": workflow_ref
                }
            }
        })
    }
    #[tokio::test]
    async fn atom_invoke_deterministic_runner_returns_terminal_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "echo-atom.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            deterministic_echo_atom("echo-atom"),
        )
        .await
        .unwrap();

        let invoke = server
            .atom_invoke(Parameters(AtomInvokeParams {
                atom: "atom:echo-atom@v1".into(),
                args: serde_json::json!({"message": "hello"}),
                project_dir: None,
                owner: Some("operator:test".into()),
                parent_invocation_id: None,
                runtime: None,
                supervision_override: None,
                suppress_auto_supervision: false,
            }))
            .await;
        assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["data"]["echo"]["message"], "hello");
        assert_eq!(body["output_shape"]["valid"], true);

        let status = server.atom_status(Parameters(AtomStatusParams {
            invocation_id: body["invocation_id"].as_str().unwrap().to_string(),
            owner: Some("operator:test".into()),
        }));
        assert_ne!(status.is_error, Some(true), "{}", extract_text(&status));
        let trace: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
        assert_eq!(trace["implementation_kind"], "deterministic");
        assert_eq!(trace["state"], "succeeded");
        assert_eq!(trace["effects_observed"]["dispatches_runs"], 0);
        assert_eq!(trace["output_shape"]["valid"], true);
    }

    #[tokio::test]
    async fn atom_invoke_adapter_runner_returns_terminal_trace() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "badgey-adapter.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            badgey_adapter_atom("badgey-adapter"),
        )
        .await
        .unwrap();

        let invoke = server
            .atom_invoke(Parameters(AtomInvokeParams {
                atom: "atom:badgey-adapter@v1".into(),
                args: serde_json::json!({"brief": "hello badgey"}),
                project_dir: None,
                owner: Some("operator:test".into()),
                parent_invocation_id: None,
                runtime: None,
                supervision_override: None,
                suppress_auto_supervision: false,
            }))
            .await;
        assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
        assert_eq!(body["status"], "succeeded");
        assert_eq!(body["data"]["adapter"], "badgey");
        assert_eq!(body["data"]["accepted"], true);
        assert_eq!(body["output_shape"]["valid"], true);
    }

    #[tokio::test]
    async fn shipped_refactor_atom_installs_after_persona_brofile() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let brofile: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/brofiles/refactor/rust-refactor-persona.json"
        ))
        .unwrap();
        let atom: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/atoms/refactor/rust-test-island-extract.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: "system-defaults/brofiles/refactor/rust-refactor-persona.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            brofile,
        )
        .await
        .unwrap();
        let meta = install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "system-defaults/atoms/refactor/rust-test-island-extract.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            atom,
        )
        .await
        .unwrap();

        assert_eq!(meta.kind, artifacts::ArtifactKind::Atom);
        assert_eq!(meta.name, "rust-test-island-extract");
        assert!(meta.active);
    }

    #[tokio::test]
    async fn shipped_rust_batch2_atoms_install_after_persona_brofile() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let brofile: serde_json::Value = serde_json::from_str(include_str!(
            "../../system-defaults/brofiles/refactor/rust-refactor-persona.json"
        ))
        .unwrap();

        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Brofile,
                source: "system-defaults/brofiles/refactor/rust-refactor-persona.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            brofile,
        )
        .await
        .unwrap();

        let atoms = [
            (
                "system-defaults/atoms/refactor/rust-rename-symbol.json",
                "rust-rename-symbol",
                include_str!("../../system-defaults/atoms/refactor/rust-rename-symbol.json"),
            ),
            (
                "system-defaults/atoms/refactor/rust-extract-to-submodule.json",
                "rust-extract-to-submodule",
                include_str!("../../system-defaults/atoms/refactor/rust-extract-to-submodule.json"),
            ),
            (
                "system-defaults/atoms/refactor/rust-organize-imports.json",
                "rust-organize-imports",
                include_str!("../../system-defaults/atoms/refactor/rust-organize-imports.json"),
            ),
            (
                "system-defaults/atoms/refactor/rust-cargo-add-dep.json",
                "rust-cargo-add-dep",
                include_str!("../../system-defaults/atoms/refactor/rust-cargo-add-dep.json"),
            ),
        ];

        for (source, expected_name, body) in atoms {
            let atom: serde_json::Value = serde_json::from_str(body).unwrap();
            let meta = install_artifact_value(
                &server.state,
                ArtifactInstallParams {
                    kind: artifacts::ArtifactKind::Atom,
                    source: source.into(),
                    name: None,
                    version: None,
                    supersedes: None,
                },
                atom,
            )
            .await
            .unwrap();

            assert_eq!(meta.kind, artifacts::ArtifactKind::Atom);
            assert_eq!(meta.name, expected_name);
            assert!(meta.active);
        }
    }

    #[tokio::test]
    async fn atom_invoke_workflow_wrapper_returns_workflow_handle() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_json = r#"{
        "name": "hook-workflow",
        "version": 1,
        "actors": {},
        "nodes": {
            "Done": {
                "prompt": "workflow complete",
                "next": {"type": "terminal"}
            }
        },
        "start": "Done"
    }"#;
        let workflow_spec = workflow::load_workflow(workflow_json).unwrap();
        server
            .state
            .workflow_registry
            .write()
            .insert("hook-workflow".into(), workflow_spec);
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "workflow-wrapper.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_wrapper_atom("workflow-wrapper", "workflow:hook-workflow@v1"),
        )
        .await
        .unwrap();

        let invoke = server
            .atom_invoke(Parameters(AtomInvokeParams {
                atom: "atom:workflow-wrapper@v1".into(),
                args: serde_json::json!({}),
                project_dir: None,
                owner: Some("operator:test".into()),
                parent_invocation_id: None,
                runtime: None,
                supervision_override: None,
                suppress_auto_supervision: false,
            }))
            .await;
        assert_ne!(invoke.is_error, Some(true), "{}", extract_text(&invoke));
        let body: serde_json::Value = serde_json::from_str(&extract_text(&invoke)).unwrap();
        let task_id = body["task_id"].as_str().unwrap().to_string();
        let task = server.state.task_store.read().get(&task_id).unwrap();
        assert!(orchestration::wait_for_task_with_timeout(&task, Some(2.0)).await);

        let status = server.atom_status(Parameters(AtomStatusParams {
            invocation_id: body["invocation_id"].as_str().unwrap().to_string(),
            owner: Some("operator:test".into()),
        }));
        assert_ne!(status.is_error, Some(true), "{}", extract_text(&status));
        let trace: serde_json::Value = serde_json::from_str(&extract_text(&status)).unwrap();
        assert_eq!(trace["implementation_kind"], "workflow");
        assert_eq!(trace["state"], "succeeded");
        assert_eq!(trace["cost"]["dispatched_runs"], 1);
    }

    #[tokio::test]
    async fn workflow_atom_rejects_underdeclared_raw_actor_dispatch_budget() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let workflow_json = r#"{
        "name": "actor-workflow",
        "version": 1,
        "actors": {
            "worker": {"kind": "executor", "brofile": "missing-worker"}
        },
        "nodes": {
            "Work": {
                "actor": "worker",
                "next": {"type": "terminal"}
            }
        },
        "start": "Work"
    }"#;
        server.state.workflow_registry.write().insert(
            "actor-workflow".into(),
            workflow::load_workflow(workflow_json).unwrap(),
        );
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "underdeclared-workflow.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            workflow_wrapper_atom("underdeclared-workflow", "workflow:actor-workflow@v1"),
        )
        .await
        .unwrap();

        let invoke = server
            .atom_invoke(Parameters(AtomInvokeParams {
                atom: "atom:underdeclared-workflow@v1".into(),
                args: serde_json::json!({}),
                project_dir: None,
                owner: Some("operator:test".into()),
                parent_invocation_id: None,
                runtime: None,
                supervision_override: None,
                suppress_auto_supervision: false,
            }))
            .await;
        assert_eq!(invoke.is_error, Some(true));
        assert!(extract_text(&invoke).contains("dispatches_runs_exhausted"));
    }

    #[tokio::test]
    async fn atom_binding_workflow_invokes_deterministic_atom() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "echo-atom.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            deterministic_echo_atom("workflow-echo"),
        )
        .await
        .unwrap();

        let workflow_json = r#"{
        "name": "workflow-atom-binding-runtime",
        "version": 1,
        "actors": {},
        "vars_schema": {
            "message": {"kind": "string"}
        },
        "atom_bindings": {
            "echo": {
                "atom_ref": "atom:workflow-echo@v1",
                "limits": {"dispatches_runs": 0}
            }
        },
        "nodes": {
            "Echo": {
                "atom": "echo",
                "atom_args": {"message": "${vars.message}"},
                "next": {"type": "terminal"}
            }
        },
        "start": "Echo"
    }"#;
        let spec = workflow::load_workflow(workflow_json).unwrap();
        let compiled = workflow::compile(spec).unwrap();
        validate_workflow_capabilities(&compiled, &server.state).unwrap();
        let result = workflow::run_workflow_with_initial_vars(
            &server,
            &compiled,
            None,
            Some(10),
            serde_json::Map::from_iter([(
                "message".to_string(),
                serde_json::Value::String("from workflow".into()),
            )]),
        )
        .await;
        assert_eq!(result.status, "completed");
        let output: serde_json::Value = serde_json::from_str(&result.node_outputs["Echo"]).unwrap();
        assert_eq!(output["implementation_kind"], "deterministic");
        assert_eq!(output["state"], "succeeded");
    }

    #[tokio::test]
    async fn atom_install_rejects_unknown_deterministic_runner() {
        let tmp = tempfile::tempdir().unwrap();
        let server = test_server(&tmp);
        let mut atom = deterministic_echo_atom("bad-runner");
        atom["manifest"]["implementation"]["runner"] = serde_json::json!("missing-runner");
        let result = install_artifact_value(
            &server.state,
            ArtifactInstallParams {
                kind: artifacts::ArtifactKind::Atom,
                source: "bad-runner.json".into(),
                name: None,
                version: None,
                supersedes: None,
            },
            atom,
        )
        .await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("unknown deterministic")
        );
    }
}

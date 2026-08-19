mod composition;
mod helpers;
mod invoke;
mod supervision;
#[cfg(test)]
mod tests;
use crate::orchestration;
use crate::orchestration::providers::dispatch_prelude::*;
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

fn atom_route_handle(name: &str, version: &str) -> String {
    format!("atom:{name}@v{version}")
}

fn atom_route_fit(rank: usize) -> &'static str {
    match rank {
        0 => "high",
        1 => "medium",
        _ => "fallback",
    }
}

fn atom_route_next() -> serde_json::Value {
    serde_json::json!([
        {
            "tool": "atom_describe",
            "reason": "review schema/effects"
        }
    ])
}

fn atom_required_inputs(manifest: &orchestration::atoms::types::AtomManifest) -> Vec<String> {
    manifest
        .inputs
        .as_ref()
        .and_then(|inputs| inputs.schema.as_ref())
        .and_then(|schema| schema.get("required"))
        .and_then(serde_json::Value::as_array)
        .map(|required| {
            required
                .iter()
                .filter_map(|field| field.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn normalized_field_text(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn compact_field_text(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn query_supplies_field(query: &str, field: &str) -> bool {
    let query_lower = query.to_ascii_lowercase();
    if query_lower.contains(&field.to_ascii_lowercase()) {
        return true;
    }
    let query_words = normalized_field_text(query);
    let field_words = normalized_field_text(field);
    if !field_words.is_empty() && query_words.contains(&field_words) {
        return true;
    }
    let query_compact = compact_field_text(query);
    let field_compact = compact_field_text(field);
    !field_compact.is_empty() && query_compact.contains(&field_compact)
}

fn atom_missing_facts(
    query: &str,
    manifest: &orchestration::atoms::types::AtomManifest,
) -> Vec<String> {
    atom_required_inputs(manifest)
        .into_iter()
        .filter(|field| !query_supplies_field(query, field))
        .collect()
}

fn atom_stop_if(_manifest: &orchestration::atoms::types::AtomManifest) -> Vec<String> {
    Vec::new()
}

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
                    orchestration::atoms::types::AtomImplementation::Consultant { .. } => {
                        "consultant".to_string()
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
        description = "Search installed atoms by query string. Matches against description and when_to_use; penalizes or excludes results matching anti_patterns. Returns ranked results with scores, provenance, and v1 route-card fields: handle, kind, fit, next, missing_facts, stop_if."
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
            .enumerate()
            .map(|(rank, r)| {
                let mut obj = serde_json::json!({
                    "name": r.name,
                    "version": r.version,
                    "score": (r.score * 1000.0).round() / 1000.0,
                    "handle": atom_route_handle(&r.name, &r.version),
                    "kind": "existing_atom",
                    "fit": atom_route_fit(rank),
                    "next": atom_route_next(),
                    "missing_facts": atom_missing_facts(query, &r.manifest),
                    "stop_if": atom_stop_if(&r.manifest),
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
        // External atom invocations are fresh work: refused while draining.
        // Internal callers (workflow atom nodes, auto-supervision) use
        // `atom_invoke_value` directly and are in-flight work.
        if let Some(refusal) = self.state.drain.admission_refusal("atom_invoke") {
            return Self::err_text(&refusal);
        }
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
        } else if bf.model.is_some()
            || bf.effort.is_some()
            || bf.code_mode.is_some()
            || bf.service_tier.is_some()
        {
            Some(ExecOpts {
                model: bf.model.clone(),
                effort: bf.effort.clone(),
                provider_defaults: None,
                code_mode: bf.code_mode,
                service_tier: bf.service_tier.clone(),
                output_schema: None,
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
        // Full dispatch context on resume, persona included
        // (dispatch-prompt-slots.md §6).
        let dispatch_context = ambient_ctx.dispatch_context(bf.lens.as_deref());

        let mut args = provider.build_resume_args(
            &session_id,
            &p.prompt,
            Some(&dispatch_context),
            exec_opts.as_ref(),
        );
        // Fold the brofile's surface-packet verdict into the dispatch filter
        // plane (disallow-wins), matching the authority applied by the MCP wire
        // head used by the harness child.
        let surface_filters = crate::server::surface::dispatch_surface_filters(
            &self.state.packets.read(),
            bf.surface.as_deref(),
            resume_cwd.as_deref(),
        );
        let brofile_filters = crate::server::progress::combine_dispatch_filters(
            bf.filters.as_ref(),
            surface_filters.as_ref(),
        );
        let dispatch_filters = match crate::server::progress::resolve_dispatch_filters(
            provider,
            resume_cwd.as_deref(),
            false,
            &task_id_new,
            brofile_filters.as_ref(),
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
            Some(self.state.roster_events()),
            Some(inv.atom_ref.clone()),
            Some(inv.atom_ref.clone()),
            Some(self.state.system_events.clone()),
            // atom_resume_value is the resume half of atom
            // dispatch — same source class as atom_invoke (Atom).
            bro_core::Origin::Atom,
        )
        .await;
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

mod composition;
mod helpers;
mod supervision;
use crate::server::*;
use crate::*;
#[cfg(test)]
use helpers::atom_ref_allowed;
use helpers::{
    default_atom_owner, iso_from_millis, sha256_json_value, sha256_text, validate_atom_output,
};

pub(crate) fn router() -> ToolRouter<BlackboxServer> {
    BlackboxServer::atoms_tools()
}

enum RunnerInvocationKind {
    Deterministic(String),
    Adapter(String),
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

    fn resolve_active_atom_manifest(
        &self,
        atom: &str,
    ) -> Result<(String, String, orchestration::atoms::types::AtomManifest), String> {
        use orchestration::atoms::registry::AtomRegistry;

        let catalog = self.state.artifacts.read();
        let reg = AtomRegistry::new(&catalog);
        let rec = match reg.get(atom) {
            Ok(Some(r)) if r.active => r,
            Ok(Some(_)) => return Err("atom is not active".into()),
            Ok(None) => return Err(format!("atom not found: {atom}")),
            Err(e) => return Err(format!("atom lookup failed: {e}")),
        };
        let manifest = rec.manifest.ok_or_else(|| {
            format!(
                "atom manifest parse error: {}",
                rec.manifest_parse_error.unwrap_or_default()
            )
        })?;
        Ok((rec.name.clone(), rec.version.clone(), manifest))
    }

    fn validate_atom_args(
        manifest: &orchestration::atoms::types::AtomManifest,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let args_to_validate = if args.is_null() {
            serde_json::json!({})
        } else {
            args.clone()
        };
        if let Some(inputs) = &manifest.inputs
            && let Some(schema) = &inputs.schema
        {
            let compiled = jsonschema::JSONSchema::options()
                .with_draft(jsonschema::Draft::Draft202012)
                .compile(schema)
                .map_err(|e| format!("error.internal(code=invalid_schema): manifest schema failed to compile: {e}"))?;
            if let Err(errors) = compiled.validate(&args_to_validate) {
                let msgs = errors.map(|e| e.to_string()).collect::<Vec<_>>();
                return Err(format!(
                    "error.bad_input(code=schema_validation_failed): {}",
                    msgs.join("; ")
                ));
            }
        }
        Ok(args_to_validate)
    }

    pub(crate) async fn atom_invoke_value(
        &self,
        p: AtomInvokeParams,
        binding_limits: Option<&workflow::AtomBindingLimits>,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::types::AtomImplementation;

        let (atom_name, atom_version, manifest) = match self.resolve_active_atom_manifest(&p.atom) {
            Ok(found) => found,
            Err(e) => return Err(e),
        };
        let atom_ref = format!("atom:{atom_name}@v{atom_version}");
        let args_to_validate = match Self::validate_atom_args(&manifest, &p.args) {
            Ok(args) => args,
            Err(e) => return Err(e),
        };
        if matches!(
            &manifest.implementation,
            AtomImplementation::Workflow { .. }
        ) {
            let plan = self.normalized_supervision_plan_for_invoke(
                &manifest,
                p.supervision_override.as_ref(),
            )?;
            if plan.classifier.mode != orchestration::atoms::types::SupervisionClassifierMode::None
                || plan.advisor.mode != orchestration::atoms::types::SupervisionAdvisorMode::None
            {
                return Err(
                    "error.unsupported_supervision: workflow-backed primary atoms cannot be supervised yet"
                        .into(),
                );
            }
        }

        let invocation_id = uuid::Uuid::new_v4().to_string();
        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let dispatch_cost = self.atom_dispatch_cost_for_manifest(&manifest)?;
        let (dispatch_cost, effective_limits) = self.validate_atom_invoke_policy(
            &atom_ref,
            &manifest,
            &p,
            &owner,
            dispatch_cost,
            binding_limits,
        )?;
        let input_digest = Some(sha256_json_value(&args_to_validate));
        let runtime_override = match &p.runtime {
            Some(value) => {
                match serde_json::from_value::<orchestration::allocator::RuntimeRequest>(
                    value.clone(),
                ) {
                    Ok(runtime) => Some(runtime),
                    Err(e) => {
                        return Err(format!(
                            "error.bad_input(code=invalid_runtime): runtime must be a RuntimeRequest object: {e}"
                        ));
                    }
                }
            }
            None => None,
        };

        match &manifest.implementation {
            AtomImplementation::Profile { brofile_ref } => {
                self.atom_invoke_profile(
                    &invocation_id,
                    &atom_ref,
                    &manifest,
                    brofile_ref,
                    &p,
                    &args_to_validate,
                    &owner,
                    input_digest,
                    dispatch_cost,
                    effective_limits,
                    runtime_override,
                )
                .await
            }
            AtomImplementation::Workflow { workflow_ref } => {
                if runtime_override.is_some() {
                    return Err(
                        "error.unsupported(code=runtime_with_workflow_atom): runtime overrides require a profile-backed atom"
                            .into(),
                    );
                }
                self.atom_invoke_workflow(
                    &invocation_id,
                    &atom_ref,
                    workflow_ref,
                    &p,
                    &args_to_validate,
                    &owner,
                    input_digest,
                    dispatch_cost,
                    effective_limits,
                )
                .await
            }
            AtomImplementation::Deterministic { runner } => {
                if runtime_override.is_some() {
                    return Err(
                        "error.unsupported(code=runtime_with_runner_atom): runtime overrides require a profile-backed atom"
                            .into(),
                    );
                }
                self.atom_invoke_runner(
                    &invocation_id,
                    &atom_ref,
                    &manifest,
                    RunnerInvocationKind::Deterministic(runner.clone()),
                    &p,
                    &args_to_validate,
                    &owner,
                    input_digest,
                    dispatch_cost,
                    effective_limits,
                )
            }
            AtomImplementation::Adapter { adapter_name } => {
                if runtime_override.is_some() {
                    return Err(
                        "error.unsupported(code=runtime_with_runner_atom): runtime overrides require a profile-backed atom"
                            .into(),
                    );
                }
                self.atom_invoke_runner(
                    &invocation_id,
                    &atom_ref,
                    &manifest,
                    RunnerInvocationKind::Adapter(adapter_name.clone()),
                    &p,
                    &args_to_validate,
                    &owner,
                    input_digest,
                    dispatch_cost,
                    effective_limits,
                )
            }
        }
    }

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

    async fn atom_invoke_profile(
        &self,
        invocation_id: &str,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        brofile_ref: &str,
        p: &AtomInvokeParams,
        args_to_validate: &serde_json::Value,
        owner: &str,
        input_digest: Option<String>,
        dispatch_cost: u64,
        effective_limits: orchestration::atoms::invocation::InvocationLimits,
        runtime_override: Option<orchestration::allocator::RuntimeRequest>,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::AtomInvocation;

        let typed_name =
            match orchestration::atoms::validate::parse_typed_ref(brofile_ref, "brofile:") {
                Ok((name, _ver)) => name,
                Err(e) => return Err(format!("invalid brofile_ref: {e}")),
            };

        let bf = match orchestration::brofile::resolve_brofile(
            &typed_name,
            &self.state.store_dir,
            p.project_dir.as_deref(),
        ) {
            Some(b) => b,
            None => return Err(format!("brofile '{}' not found", typed_name)),
        };

        let (base_allow, base_disallow) = match &bf.filters {
            Some(f) => (f.allow.clone(), f.disallow.clone()),
            None => (Vec::new(), Vec::new()),
        };
        orchestration::brofile::enforce_provider_defaults(bf.provider, bf.context.as_ref())?;
        let env_overrides = orchestration::brofile::resolve_provider_env(
            bf.provider,
            bf.account.as_deref(),
            bf.model.as_deref(),
            &self.state.store_dir,
            bf.context.as_ref(),
        );
        let exec_opts = if bf.model.is_some() || bf.effort.is_some() {
            Some(orchestration::providers::ExecOpts {
                model: bf.model.clone(),
                effort: bf.effort.clone(),
                provider_defaults: None,
            })
        } else {
            None
        };
        let exec_opts = orchestration::providers::exec_opts_with_provider_defaults(
            exec_opts,
            bf.context.as_ref(),
        );

        let prompt = match (&manifest.inputs, args_to_validate) {
            (Some(spec), _) if spec.prompt_template.is_some() => {
                Self::expand_template(spec.prompt_template.as_ref().unwrap(), args_to_validate)
            }
            _ => {
                if args_to_validate.is_null() {
                    String::new()
                } else {
                    serde_json::to_string_pretty(args_to_validate).unwrap_or_default()
                }
            }
        };
        let supervision_plan =
            self.normalized_supervision_plan_for_invoke(manifest, p.supervision_override.as_ref())?;

        let cwd = p.project_dir.clone();
        let brofile_filters = orchestration::mcp::McpFilters {
            allow: base_allow,
            disallow: base_disallow,
        };
        let atom_label = atom_ref.to_string();
        let runtime =
            orchestration::allocator::merge_runtime_request(bf.runtime, manifest.runtime.clone());
        let runtime = orchestration::allocator::merge_runtime_request(runtime, runtime_override);
        let runtime = if manifest
            .outputs
            .as_ref()
            .and_then(|outputs| outputs.schema.as_ref())
            .is_some()
        {
            orchestration::allocator::with_derived_capability(
                runtime,
                orchestration::providers::Capability::StructuredOutput,
            )
        } else {
            runtime
        };
        let dispatched =
            self.dispatch_fresh_bro_task(crate::tools::dispatch::FreshDispatchRequest {
                prompt,
                provider: bf.provider,
                lens: bf.lens,
                exec_opts,
                env_overrides,
                cwd: cwd.clone(),
                brofile_filters: Some(brofile_filters),
                coerce_workspace: bf.coerce_workspace.unwrap_or(false),
                allow_recursion: false,
                allow_tools: None,
                disallow_tools: None,
                surface: None,
                allocation_request: runtime,
                project_dir_for_lease: p.project_dir.clone(),
                ambient_bro_name: Some(atom_label.clone()),
                spawn_bro_label: Some(atom_label.clone()),
                spawn_agent_label: Some(atom_label.clone()),
                record_to_bro: None,
                brofile_context: bf.context,
            })?;
        let (task_id, session_id, selected_provider) = {
            let inner = dispatched.task.inner.lock();
            (inner.id.clone(), inner.session_id.clone(), inner.provider)
        };

        let mut inv = AtomInvocation::new_profile(
            invocation_id.to_string(),
            atom_ref.to_string(),
            p.parent_invocation_id.clone(),
            owner.to_string(),
            selected_provider.as_str().to_string(),
            session_id.clone(),
            cwd,
            task_id.clone(),
        );
        inv.input_digest = input_digest;
        inv.effective_limits = Some(effective_limits);
        inv.effects_observed.dispatches_runs = Some(dispatch_cost);
        inv.cost.dispatched_runs = Some(dispatch_cost);
        self.state.atom_invocation_store.write().insert(inv);
        self.record_child_invocation(
            p.parent_invocation_id.as_deref(),
            invocation_id,
            dispatch_cost,
        );
        let supervision = if p.suppress_auto_supervision {
            serde_json::json!({"enabled": false, "suppressed": true})
        } else {
            self.start_supervision_for_primary_invocation(
                invocation_id,
                &task_id,
                owner,
                p.project_dir.clone(),
                &supervision_plan,
            )
            .await?
        };

        Ok(serde_json::json!({
            "invocation_id": invocation_id,
            "atom_ref": atom_ref,
            "task_id": task_id,
            "session_id": session_id,
            "status": "running",
            "supervision": supervision,
        }))
    }

    async fn atom_invoke_workflow(
        &self,
        invocation_id: &str,
        atom_ref: &str,
        workflow_ref: &str,
        p: &AtomInvokeParams,
        args_to_validate: &serde_json::Value,
        owner: &str,
        input_digest: Option<String>,
        dispatch_cost: u64,
        effective_limits: orchestration::atoms::invocation::InvocationLimits,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::AtomInvocation;

        let (workflow_name, workflow_version) =
            match orchestration::atoms::validate::parse_typed_ref(workflow_ref, "workflow:") {
                Ok(parsed) => parsed,
                Err(e) => return Err(format!("invalid workflow_ref: {e}")),
            };
        let workflow = self
            .resolve_workflow_by_id(&workflow_name)
            .or_else(|| self.resolve_workflow_by_id(workflow_ref))
            .ok_or_else(|| {
                format!(
                    "workflow '{}' not found for atom {}",
                    workflow_ref, atom_ref
                )
            })?;
        if let Some(pinned) = workflow_version.strip_prefix('v') {
            let expected = pinned.parse::<u32>().map_err(|_| {
                format!("invalid workflow_ref version in '{workflow_ref}': {workflow_version}")
            })?;
            if workflow.version != expected {
                return Err(format!(
                    "workflow_ref {workflow_ref} resolved to workflow version {}, expected {expected}",
                    workflow.version
                ));
            }
        }

        let compiled = workflow::compile(workflow)
            .map_err(|e| format!("workflow-backed atom compile failed: {e}"))?;
        validate_workflow_capabilities(&compiled, &self.state)
            .map_err(|e| format!("workflow-backed atom capability validation failed: {e}"))?;

        let mut initial_vars = match args_to_validate {
            serde_json::Value::Object(map) => map.clone(),
            other => serde_json::Map::from_iter([("input".to_string(), other.clone())]),
        };
        initial_vars.insert(
            "_atom_parent_invocation_id".to_string(),
            serde_json::Value::String(invocation_id.to_string()),
        );
        initial_vars.insert(
            "_atom_owner".to_string(),
            serde_json::Value::String(owner.to_string()),
        );

        let (task, arc_id) =
            self.spawn_workflow_task(compiled, p.project_dir.clone(), None, initial_vars);
        let task_id = task.inner.lock().id.clone();
        let mut inv = AtomInvocation::new_workflow(
            invocation_id.to_string(),
            atom_ref.to_string(),
            p.parent_invocation_id.clone(),
            owner.to_string(),
            workflow_ref.to_string(),
            Some(arc_id.clone()),
            Some(task_id.clone()),
        );
        inv.input_digest = input_digest;
        inv.effective_limits = Some(effective_limits);
        inv.effects_observed.dispatches_runs = Some(dispatch_cost);
        inv.cost.dispatched_runs = Some(dispatch_cost);
        self.state.atom_invocation_store.write().insert(inv);
        self.record_child_invocation(
            p.parent_invocation_id.as_deref(),
            invocation_id,
            dispatch_cost,
        );

        Ok(serde_json::json!({
            "invocation_id": invocation_id,
            "atom_ref": atom_ref,
            "task_id": task_id,
            "arc_id": arc_id,
            "status": "running",
        }))
    }

    fn atom_invoke_runner(
        &self,
        invocation_id: &str,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        kind: RunnerInvocationKind,
        p: &AtomInvokeParams,
        args_to_validate: &serde_json::Value,
        owner: &str,
        input_digest: Option<String>,
        dispatch_cost: u64,
        effective_limits: orchestration::atoms::invocation::InvocationLimits,
    ) -> Result<serde_json::Value, String> {
        use orchestration::atoms::invocation::{
            AtomHandle, AtomInvocation, EffectsObserved, InvocationCost, InvocationStatus,
        };
        use orchestration::atoms::runners::{RunnerStatus, default_registry};

        let registry = default_registry();
        let (handle, result) = match kind {
            RunnerInvocationKind::Deterministic(runner) => {
                let result = registry.execute_deterministic(&runner, args_to_validate);
                (AtomHandle::Deterministic { runner }, result)
            }
            RunnerInvocationKind::Adapter(adapter_name) => {
                let result = registry.execute_adapter(&adapter_name, args_to_validate);
                let adapter_handle = result
                    .data
                    .get("handle")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string);
                (
                    AtomHandle::Adapter {
                        adapter_name,
                        adapter_handle,
                    },
                    result,
                )
            }
        };

        let status = match result.status {
            RunnerStatus::Succeeded => InvocationStatus::Succeeded,
            RunnerStatus::Failed => InvocationStatus::Failed,
        };
        let output_shape = validate_atom_output(manifest, &result.data);
        let output_text = serde_json::to_string(&result.data).unwrap_or_default();
        let mut owners = std::collections::HashSet::new();
        owners.insert(owner.to_string());
        let inv = AtomInvocation {
            invocation_id: invocation_id.to_string(),
            atom_ref: atom_ref.to_string(),
            parent_invocation_id: p.parent_invocation_id.clone(),
            owners,
            handle,
            status: status.clone(),
            started_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            ended_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            input_digest,
            output_digest: Some(sha256_json_value(&result.data)),
            output_shape: Some(output_shape.clone()),
            structured_output: None,
            effective_limits: Some(effective_limits),
            summary: Some(output_text.chars().take(500).collect()),
            effects_observed: EffectsObserved {
                dispatches_runs: Some(dispatch_cost),
                ..EffectsObserved::default()
            },
            cost: InvocationCost {
                dispatched_runs: Some(dispatch_cost),
                wall_time_ms: Some(result.wall_time_ms),
                ..InvocationCost::default()
            },
            children: Vec::new(),
            errors: result.errors.clone(),
            artifacts: Vec::new(),
        };
        self.state.atom_invocation_store.write().insert(inv);
        self.record_child_invocation(
            p.parent_invocation_id.as_deref(),
            invocation_id,
            dispatch_cost,
        );

        Ok(serde_json::json!({
            "invocation_id": invocation_id,
            "atom_ref": atom_ref,
            "status": match status {
                InvocationStatus::Succeeded => "succeeded",
                InvocationStatus::Failed => "failed",
                _ => "running",
            },
            "data": result.data,
            "output_shape": output_shape,
            "errors": result.errors,
        }))
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
    use crate::tools::atoms::helpers::bounded_effect_u64;
    use std::sync::Arc;

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
                .create(&crate::NoteParams {
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
}

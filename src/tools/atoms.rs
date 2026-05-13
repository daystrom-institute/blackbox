use crate::server::*;
use crate::*;

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

    fn validate_atom_invoke_policy(
        &self,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        p: &AtomInvokeParams,
        owner: &str,
    ) -> Result<u64, String> {
        use orchestration::atoms::types::{AtomImplementation, MayInvokeAtoms};

        let dispatch_cost = atom_dispatch_cost(&manifest.implementation);
        if let Some(effects) = &manifest.effects
            && let Some(limit) = bounded_effect_u64(effects.dispatches_runs.as_ref())?
            && dispatch_cost > limit
        {
            return Err(format!(
                "error.policy(code=dispatches_runs_exhausted): atom declares dispatches_runs={limit} but invocation requires {dispatch_cost}"
            ));
        }

        let Some(parent_id) = p.parent_invocation_id.as_deref() else {
            return Ok(dispatch_cost);
        };

        let ancestors = self.atom_invocation_ancestors(parent_id)?;
        let Some(parent) = ancestors.first() else {
            return Err(format!(
                "error.policy(code=parent_not_found): parent invocation not found: {parent_id}"
            ));
        };
        if !parent.is_owner(owner) {
            return Err(
                "error.forbidden: caller must own the parent invocation to invoke a child atom"
                    .into(),
            );
        }

        let (_, _, parent_manifest) = self.resolve_active_atom_manifest(&parent.atom_ref)?;
        match parent_manifest
            .composition
            .as_ref()
            .map(|c| &c.may_invoke_atoms)
        {
            Some(MayInvokeAtoms::Any) => {}
            Some(MayInvokeAtoms::Allowed { atoms }) if atom_ref_allowed(atoms, atom_ref) => {}
            _ => {
                return Err(format!(
                    "error.policy(code=composition_denied): parent invocation {parent_id} may not invoke {atom_ref}"
                ));
            }
        }

        for (depth_from_ancestor, ancestor) in ancestors.iter().enumerate() {
            let (_, _, ancestor_manifest) =
                self.resolve_active_atom_manifest(&ancestor.atom_ref)?;
            if let Some(effects) = &ancestor_manifest.effects {
                if let Some(max_depth) = bounded_effect_u64(effects.max_depth.as_ref())? {
                    let requested_depth = (depth_from_ancestor + 1) as u64;
                    if requested_depth > max_depth {
                        return Err(format!(
                            "error.policy(code=depth_exhausted): ancestor {} allows max_depth={max_depth}, requested depth={requested_depth}",
                            ancestor.invocation_id
                        ));
                    }
                }
                if let Some(dispatch_limit) = bounded_effect_u64(effects.dispatches_runs.as_ref())?
                {
                    let observed = ancestor.effects_observed.dispatches_runs.unwrap_or(0);
                    if observed.saturating_add(dispatch_cost) > dispatch_limit {
                        return Err(format!(
                            "error.policy(code=budget_exhausted): ancestor {} allows dispatches_runs={dispatch_limit}, observed={observed}, requested={dispatch_cost}",
                            ancestor.invocation_id
                        ));
                    }
                }
            }
        }

        if matches!(
            manifest.implementation,
            AtomImplementation::Deterministic { .. } | AtomImplementation::Adapter { .. }
        ) {
            // Deterministic/adapter invocations may still be children; this
            // branch exists to make the zero-dispatch case explicit.
        }
        Ok(dispatch_cost)
    }

    fn atom_invocation_ancestors(
        &self,
        parent_id: &str,
    ) -> Result<Vec<orchestration::atoms::invocation::AtomInvocation>, String> {
        let store = self.state.atom_invocation_store.read();
        let mut out = Vec::new();
        let mut cursor = Some(parent_id.to_string());
        let mut guard = 0usize;
        while let Some(id) = cursor {
            guard += 1;
            if guard > 64 {
                return Err(
                    "error.policy(code=depth_cycle): invocation parent chain exceeded 64 entries"
                        .into(),
                );
            }
            let Some(inv) = store.get(&id).cloned() else {
                if out.is_empty() {
                    return Ok(out);
                }
                return Err(format!(
                    "error.policy(code=broken_parent_chain): invocation {id} is missing"
                ));
            };
            cursor = inv.parent_invocation_id.clone();
            out.push(inv);
        }
        Ok(out)
    }

    fn record_child_invocation(&self, parent_id: Option<&str>, child_id: &str, dispatch_cost: u64) {
        let Some(parent_id) = parent_id else {
            return;
        };
        let mut store = self.state.atom_invocation_store.write();
        let mut cursor = Some(parent_id.to_string());
        let mut direct = true;
        let mut guard = 0usize;
        while let Some(id) = cursor {
            guard += 1;
            if guard > 64 {
                break;
            }
            let Some(inv) = store.get_mut(&id) else {
                break;
            };
            if direct && !inv.children.iter().any(|c| c == child_id) {
                inv.children.push(child_id.to_string());
                direct = false;
            }
            let current = inv.effects_observed.dispatches_runs.unwrap_or(0);
            inv.effects_observed.dispatches_runs = Some(current.saturating_add(dispatch_cost));
            let current_cost = inv.cost.dispatched_runs.unwrap_or(0);
            inv.cost.dispatched_runs = Some(current_cost.saturating_add(dispatch_cost));
            cursor = inv.parent_invocation_id.clone();
        }
        let _ = store.persist();
    }

    #[tool(
        name = "atom_invoke",
        description = "Invoke an installed atom. Resolves the atom manifest, validates policy gates (effects, composition, depth), and dispatches via the appropriate implementation path (profile, workflow, deterministic, adapter). Returns an owned invocation handle with invocation_id and underlying task/session ids."
    )]
    pub(crate) async fn atom_invoke(
        &self,
        Parameters(p): Parameters<AtomInvokeParams>,
    ) -> CallToolResult {
        use orchestration::atoms::types::AtomImplementation;

        let (atom_name, atom_version, manifest) = match self.resolve_active_atom_manifest(&p.atom) {
            Ok(found) => found,
            Err(e) => return Self::err_text(&e),
        };
        let atom_ref = format!("atom:{atom_name}@v{atom_version}");
        let args_to_validate = match Self::validate_atom_args(&manifest, &p.args) {
            Ok(args) => args,
            Err(e) => return Self::err_text(&e),
        };

        let invocation_id = uuid::Uuid::new_v4().to_string();
        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let dispatch_cost = match self.validate_atom_invoke_policy(&atom_ref, &manifest, &p, &owner)
        {
            Ok(cost) => cost,
            Err(e) => return Self::err_text(&e),
        };
        let input_digest = Some(sha256_json_value(&args_to_validate));

        match &manifest.implementation {
            AtomImplementation::Profile { brofile_ref } => {
                self.atom_invoke_profile(
                    &invocation_id,
                    &atom_ref,
                    &manifest,
                    brofile_ref,
                    &p,
                    &owner,
                    input_digest,
                    dispatch_cost,
                )
                .await
            }
            AtomImplementation::Workflow { .. } => Self::err_text(
                "error.not_implemented(code=workflow_atom): workflow-backed atoms are not yet supported",
            ),
            AtomImplementation::Deterministic { .. } => Self::err_text(
                "error.not_implemented(code=deterministic_atom): deterministic atoms are not yet supported",
            ),
            AtomImplementation::Adapter { .. } => Self::err_text(
                "error.not_implemented(code=adapter_atom): adapter atoms are not yet supported",
            ),
        }
    }

    async fn atom_invoke_profile(
        &self,
        invocation_id: &str,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        brofile_ref: &str,
        p: &AtomInvokeParams,
        owner: &str,
        input_digest: Option<String>,
        dispatch_cost: u64,
    ) -> CallToolResult {
        use orchestration::atoms::invocation::AtomInvocation;
        use orchestration::providers::ExecOpts;

        let typed_name =
            match orchestration::atoms::validate::parse_typed_ref(brofile_ref, "brofile:") {
                Ok((name, _ver)) => name,
                Err(e) => return Self::err_text(&format!("invalid brofile_ref: {e}")),
            };

        let bf = match orchestration::brofile::resolve_brofile(
            &typed_name,
            &self.state.store_dir,
            p.project_dir.as_deref(),
        ) {
            Some(b) => b,
            None => return Self::err_text(&format!("brofile '{}' not found", typed_name)),
        };

        let (base_allow, base_disallow) = match &bf.filters {
            Some(f) => (f.allow.clone(), f.disallow.clone()),
            None => (Vec::new(), Vec::new()),
        };
        let env_overrides = orchestration::brofile::resolve_provider_env(
            bf.provider,
            bf.account.as_deref(),
            bf.model.as_deref(),
            &self.state.store_dir,
        );
        let exec_opts = if bf.model.is_some() || bf.effort.is_some() {
            Some(ExecOpts {
                model: bf.model.clone(),
                effort: bf.effort.clone(),
            })
        } else {
            None
        };

        let prompt = match (&manifest.inputs, &p.args) {
            (Some(spec), _) if spec.prompt_template.is_some() => {
                Self::expand_template(spec.prompt_template.as_ref().unwrap(), &p.args)
            }
            _ => {
                if p.args.is_null() {
                    String::new()
                } else {
                    serde_json::to_string_pretty(&p.args).unwrap_or_default()
                }
            }
        };

        let task_id = uuid::Uuid::new_v4().to_string();
        let session_id = if matches!(bf.provider, orchestration::providers::Provider::Claude) {
            uuid::Uuid::new_v4().to_string()
        } else {
            "pending".to_string()
        };
        let cwd = p.project_dir.clone();

        let atom_label = atom_ref.to_string();
        let ambient_ctx = orchestration::AmbientContext {
            task_id: Some(task_id.clone()),
            session_id: Some(session_id.clone()),
            project_dir: cwd.clone(),
            bro_name: Some(atom_label.clone()),
            thread_id: None,
            work_item_id: None,
            pin_block: self.ambient_pin_block(
                cwd.as_deref(),
                Some(atom_label.as_str()),
                Some(session_id.as_str()),
                None,
                None,
            ),
            completion_contract: Some(orchestration::DEFAULT_COMPLETION_CONTRACT.to_string()),
            allow_recursion: false,
            provider: Some(bf.provider),
            coerce_workspace: bf.coerce_workspace.unwrap_or(false),
        };
        let final_prompt = orchestration::apply_brofile_lens(
            &orchestration::apply_ambient(&prompt, &ambient_ctx),
            bf.lens.as_deref(),
        );

        let mut args = bf.provider.build_exec_args(
            &final_prompt,
            &session_id,
            cwd.as_deref(),
            exec_opts.as_ref(),
        );
        let brofile_filters = orchestration::mcp::McpFilters {
            allow: base_allow,
            disallow: base_disallow,
        };
        let extra = crate::server::progress::combine_dispatch_filters(Some(&brofile_filters), None);
        let dispatch_filters = match crate::server::progress::resolve_dispatch_filters(
            bf.provider,
            cwd.as_deref(),
            false,
            &task_id,
            extra.as_ref(),
            None,
            &self.state.packets.read(),
        ) {
            Ok(df) => df,
            Err(e) => return Self::err_text(&format!("dispatch filter resolution failed: {e}")),
        };
        args.extend(dispatch_filters.args);

        let task = orchestration::spawn_task(
            task_id.clone(),
            bf.provider,
            args,
            session_id.clone(),
            cwd.clone(),
            env_overrides,
            self.state.store_dir.clone(),
            self.state.task_store.clone(),
            self.state.tail_tx.clone(),
            Some(atom_label.clone()),
            Some(atom_label.clone()),
        );

        crate::server::progress::cleanup_policy_file_when_done(
            task.clone(),
            dispatch_filters.policy_file,
        );

        let mut inv = AtomInvocation::new_profile(
            invocation_id.to_string(),
            atom_ref.to_string(),
            p.parent_invocation_id.clone(),
            owner.to_string(),
            bf.provider.as_str().to_string(),
            session_id.clone(),
            cwd,
            task_id.clone(),
        );
        inv.input_digest = input_digest;
        inv.effects_observed.dispatches_runs = Some(dispatch_cost);
        inv.cost.dispatched_runs = Some(dispatch_cost);
        self.state.atom_invocation_store.write().insert(inv);
        self.record_child_invocation(
            p.parent_invocation_id.as_deref(),
            invocation_id,
            dispatch_cost,
        );

        Self::ok_json(&serde_json::json!({
            "invocation_id": invocation_id,
            "atom_ref": atom_ref,
            "task_id": task_id,
            "session_id": session_id,
            "status": "running",
        }))
    }

    // ── atom_status ─────────────────────────────────────────────

    fn refresh_atom_invocation_from_task(
        &self,
        inv: &mut orchestration::atoms::invocation::AtomInvocation,
    ) {
        use orchestration::atoms::invocation::{AtomHandle, InvocationStatus};

        let AtomHandle::Profile {
            task_id,
            session_id,
            ..
        } = &mut inv.handle
        else {
            return;
        };

        let task_store = self.state.task_store.read();
        let Some(task) = task_store.get(task_id) else {
            return;
        };
        let inner = task.inner.lock();
        inv.status = match inner.status {
            orchestration::TaskStatus::Completed => InvocationStatus::Succeeded,
            orchestration::TaskStatus::Failed => InvocationStatus::Failed,
            orchestration::TaskStatus::Running => InvocationStatus::Running,
            orchestration::TaskStatus::Cancelled => InvocationStatus::Cancelled,
        };
        if *session_id == "pending" && inner.session_id != "pending" {
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
        let mut inv = {
            let inv_store = self.state.atom_invocation_store.read();
            match inv_store.get(&p.invocation_id).cloned() {
                Some(i) => i,
                None => {
                    return Self::err_text(&format!("invocation not found: {}", p.invocation_id));
                }
            }
        };
        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let caller = owner.as_str();
        if !inv.is_owner(caller) {
            return Self::err_text("error.forbidden: caller is not an owner of this invocation");
        }

        {
            self.refresh_atom_invocation_from_task(&mut inv);
            self.state.atom_invocation_store.write().update(inv.clone());
        }

        Self::ok_json(&inv.to_trace_envelope())
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
        use orchestration::atoms::invocation::{AtomHandle, InvocationStatus};
        use orchestration::providers::ExecOpts;

        let owner = p.owner.clone().unwrap_or_else(default_atom_owner);
        let caller = owner.as_str();
        let mut inv = {
            let inv_store = self.state.atom_invocation_store.read();
            match inv_store.get(&p.invocation_id).cloned() {
                Some(i) => i,
                None => {
                    return Self::err_text(&format!("invocation not found: {}", p.invocation_id));
                }
            }
        };
        if !inv.is_owner(caller) {
            return Self::err_text("error.forbidden: caller is not an owner of this invocation");
        }
        self.refresh_atom_invocation_from_task(&mut inv);
        if !inv.is_resumable() {
            return Self::err_text(
                "error.not_resumable: this invocation handle does not support resume (deterministic/adapter/workflow handles are not resumable, or invocation is in a terminal non-runnable state)",
            );
        }

        let (session_id, provider_str, cwd) = match &inv.handle {
            AtomHandle::Profile {
                session_id,
                provider,
                project_dir,
                ..
            } => (session_id.clone(), provider.clone(), project_dir.clone()),
            _ => unreachable!("is_resumable checked above"),
        };
        if session_id == "pending" {
            return Self::err_text(
                "error.not_ready(code=session_pending): provider has not emitted a resumable session id yet; call atom_status again later",
            );
        }

        let provider = match provider_str.parse::<orchestration::providers::Provider>() {
            Ok(p) => p,
            Err(_) => return Self::err_text(&format!("invalid provider: {provider_str}")),
        };

        if !provider.supports_resume() {
            return Self::err_text(&format!(
                "provider '{}' does not support resume",
                provider.as_str()
            ));
        }

        let (_, _, manifest) = match self.resolve_active_atom_manifest(&inv.atom_ref) {
            Ok(found) => found,
            Err(e) => return Self::err_text(&e),
        };
        let brofile_ref = match &manifest.implementation {
            orchestration::atoms::types::AtomImplementation::Profile { brofile_ref } => brofile_ref,
            _ => {
                return Self::err_text(
                    "error.not_resumable: only profile-backed atom handles can resume through a provider session",
                );
            }
        };
        let brofile_name =
            match orchestration::atoms::validate::parse_typed_ref(brofile_ref, "brofile:") {
                Ok((name, _ver)) => name,
                Err(e) => return Self::err_text(&format!("invalid brofile_ref: {e}")),
            };
        let bf = match orchestration::brofile::resolve_brofile(
            &brofile_name,
            &self.state.store_dir,
            cwd.as_deref(),
        ) {
            Some(b) => b,
            None => return Self::err_text(&format!("brofile '{}' not found", brofile_name)),
        };
        if bf.provider != provider {
            return Self::err_text(&format!(
                "error.not_resumable(code=provider_changed): atom brofile now resolves to provider {}, but handle was created with {}",
                bf.provider.as_str(),
                provider.as_str()
            ));
        }
        let exec_opts = if bf.model.is_some() || bf.effort.is_some() {
            Some(ExecOpts {
                model: bf.model.clone(),
                effort: bf.effort.clone(),
            })
        } else {
            None
        };
        let env_overrides = orchestration::brofile::resolve_provider_env(
            bf.provider,
            bf.account.as_deref(),
            bf.model.as_deref(),
            &self.state.store_dir,
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
            Err(e) => return Self::err_text(&e),
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
            Err(e) => return Self::err_text(&format!("dispatch filter resolution failed: {e}")),
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
        );

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

        Self::ok_json(&serde_json::json!({
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

fn default_atom_owner() -> String {
    "operator:local".to_string()
}

fn atom_dispatch_cost(implementation: &orchestration::atoms::types::AtomImplementation) -> u64 {
    match implementation {
        orchestration::atoms::types::AtomImplementation::Profile { .. }
        | orchestration::atoms::types::AtomImplementation::Workflow { .. } => 1,
        orchestration::atoms::types::AtomImplementation::Deterministic { .. }
        | orchestration::atoms::types::AtomImplementation::Adapter { .. } => 0,
    }
}

fn bounded_effect_u64(value: Option<&serde_json::Value>) -> Result<Option<u64>, String> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(s)) if s == "unbounded" => Ok(None),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("invalid non-negative integer effect value: {n}")),
        Some(other) => Err(format!(
            "invalid bounded effect value (expected integer or \"unbounded\"): {other}"
        )),
    }
}

fn atom_ref_allowed(allowed: &[String], atom_ref: &str) -> bool {
    if allowed.iter().any(|candidate| candidate == atom_ref) {
        return true;
    }
    let Some(requested) = orchestration::atoms::types::AtomRef::parse(atom_ref) else {
        return false;
    };
    allowed.iter().any(|candidate| {
        let Some(candidate) = orchestration::atoms::types::AtomRef::parse(candidate) else {
            return false;
        };
        candidate.name == requested.name
            && matches!(
                candidate.version,
                orchestration::atoms::types::AtomRefVersion::Latest
            )
    })
}

fn sha256_json_value(value: &serde_json::Value) -> String {
    use sha2::Digest as _;

    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = sha2::Sha256::digest(bytes);
    format!("sha256:{}", hex::encode(digest))
}

fn sha256_text(value: &str) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(value.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

fn iso_from_millis(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

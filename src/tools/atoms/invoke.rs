use std::collections::HashSet;

use crate::server::state::BlackboxServer;
use crate::server::workflow_capabilities::validate_workflow_capabilities;
use crate::tools::atoms::helpers::{default_atom_owner, sha256_json_value, validate_atom_output};
use crate::tools::bro_params::AtomInvokeParams;
use crate::{orchestration, workflow};

enum RunnerInvocationKind {
    Deterministic(String),
    Adapter(String),
}

impl BlackboxServer {
    pub(super) fn resolve_active_atom_manifest(
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
            AtomImplementation::Consultant { consumer } => {
                if runtime_override.is_some() {
                    return Err(
                        "error.unsupported(code=runtime_with_consultant_atom): runtime overrides require a profile-backed atom"
                            .into(),
                    );
                }
                self.atom_invoke_consultant(
                    &invocation_id,
                    &atom_ref,
                    &manifest,
                    consumer,
                    &p,
                    &args_to_validate,
                    &owner,
                    input_digest,
                    dispatch_cost,
                    effective_limits,
                )
                .await
            }
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
        let exec_opts = if bf.model.is_some()
            || bf.effort.is_some()
            || bf.code_mode.is_some()
            || bf.service_tier.is_some()
        {
            Some(orchestration::providers::ExecOpts {
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
        let atom_code_mode = exec_opts.as_ref().and_then(|o| o.code_mode);
        let atom_service_tier = exec_opts.as_ref().and_then(|o| o.service_tier.clone());
        let dispatched = self
            .dispatch_fresh_bro_task(crate::tools::dispatch::FreshDispatchRequest {
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
                tool_placement: None,
                allocation_request: runtime,
                project_dir_for_lease: p.project_dir.clone(),
                ambient_bro_name: Some(atom_label.clone()),
                spawn_bro_label: Some(atom_label.clone()),
                spawn_agent_label: Some(atom_label.clone()),
                display_name: None,
                record_to_bro: None,
                brofile_context: bf.context,
                code_mode: atom_code_mode,
                service_tier: atom_service_tier,
                output_schema: None,
                // atom_invoke dispatches a catalog atom — that is the
                // source class, not a generic agent dispatch.
                origin: bro_core::Origin::Atom,
            })
            .await?;
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
        let mut owners = HashSet::new();
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

    /// One consultant TURN per invocation. Args without `consultant_id`
    /// open a new instance of the consumer (using `brief`/`prompt` as the
    /// initial brief); args with `consultant_id` resume that instance for
    /// one turn (requires `prompt`). The instance outlives the invocation
    /// — atom_status reports the turn, not the instance lifetime
    /// (consultant-runtime.md §4.10).
    #[allow(clippy::too_many_arguments)]
    async fn atom_invoke_consultant(
        &self,
        invocation_id: &str,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        consumer: &str,
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

        let descriptor =
            orchestration::consultant::consumers::lookup(consumer).ok_or_else(|| {
                format!(
                    "error.bad_input(code=unknown_consumer): no consultant consumer '{consumer}' (known: {})",
                    orchestration::consultant::consumers::names().join(", ")
                )
            })?;
        let requested_id = args_to_validate
            .get("consultant_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let prompt = args_to_validate
            .get("prompt")
            .and_then(serde_json::Value::as_str);
        let brief = args_to_validate
            .get("brief")
            .and_then(serde_json::Value::as_str);
        let project_dir = args_to_validate
            .get("project_dir")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
        let timeout_seconds = args_to_validate
            .get("timeout_seconds")
            .and_then(serde_json::Value::as_f64);

        let started_at = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        let start = std::time::Instant::now();
        let turn = match &requested_id {
            Some(raw_id) => {
                let prompt = prompt.ok_or_else(|| {
                    "error.bad_input(code=missing_prompt): a consultant turn with consultant_id requires `prompt`"
                        .to_string()
                })?;
                self.consultant_resume_internal(descriptor, raw_id, prompt, timeout_seconds)
                    .await
            }
            None => {
                let brief = brief.or(prompt).map(str::to_string);
                self.consultant_exec_internal(
                    descriptor,
                    project_dir,
                    brief,
                    Some(descriptor.agent_ref.to_string()),
                )
                .await
            }
        };
        let (status, data, errors) = match turn {
            Ok(value) => (InvocationStatus::Succeeded, value, Vec::new()),
            Err(e) => (
                InvocationStatus::Failed,
                serde_json::json!({ "error": e }),
                vec![e],
            ),
        };
        let consultant_id = data
            .get("consultant_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .or(requested_id)
            .unwrap_or_default();
        let handle = AtomHandle::Consultant {
            consumer: consumer.to_string(),
            consultant_id,
            session_id: data
                .get("session_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            task_id: data
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
        };
        let output_shape = validate_atom_output(manifest, &data);
        let output_text = serde_json::to_string(&data).unwrap_or_default();
        let mut owners = HashSet::new();
        owners.insert(owner.to_string());
        let inv = AtomInvocation {
            invocation_id: invocation_id.to_string(),
            atom_ref: atom_ref.to_string(),
            parent_invocation_id: p.parent_invocation_id.clone(),
            owners,
            handle,
            status: status.clone(),
            started_at,
            ended_at: Some(chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            input_digest,
            output_digest: Some(sha256_json_value(&data)),
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
                wall_time_ms: Some(start.elapsed().as_millis() as u64),
                ..InvocationCost::default()
            },
            children: Vec::new(),
            errors: errors.clone(),
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
            "data": data,
            "output_shape": output_shape,
            "errors": errors,
        }))
    }
}

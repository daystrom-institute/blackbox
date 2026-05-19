use crate::orchestration;
use crate::server::state::BlackboxServer;
use crate::tools::bro_params::AtomInvokeParams;
use crate::workflow;

use super::helpers::{atom_ref_allowed, effective_invocation_limits};

impl BlackboxServer {
    pub(super) fn validate_atom_invoke_policy(
        &self,
        atom_ref: &str,
        manifest: &orchestration::atoms::types::AtomManifest,
        p: &AtomInvokeParams,
        owner: &str,
        dispatch_cost: u64,
        binding_limits: Option<&workflow::AtomBindingLimits>,
    ) -> Result<(u64, orchestration::atoms::invocation::InvocationLimits), String> {
        use orchestration::atoms::types::{AtomImplementation, MayInvokeAtoms};

        let effective_limits =
            effective_invocation_limits(manifest.effects.as_ref(), binding_limits)?;
        if let Some(limit) = effective_limits.dispatches_runs
            && dispatch_cost > limit
        {
            return Err(format!(
                "error.policy(code=dispatches_runs_exhausted): atom declares dispatches_runs={limit} but invocation requires {dispatch_cost}"
            ));
        }

        let Some(parent_id) = p.parent_invocation_id.as_deref() else {
            return Ok((dispatch_cost, effective_limits));
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
            let limits = if let Some(limits) = &ancestor.effective_limits {
                limits.clone()
            } else {
                let (_, _, ancestor_manifest) =
                    self.resolve_active_atom_manifest(&ancestor.atom_ref)?;
                effective_invocation_limits(ancestor_manifest.effects.as_ref(), None)?
            };
            if let Some(max_depth) = limits.max_depth {
                let requested_depth = (depth_from_ancestor + 1) as u64;
                if requested_depth > max_depth {
                    return Err(format!(
                        "error.policy(code=depth_exhausted): ancestor {} allows max_depth={max_depth}, requested depth={requested_depth}",
                        ancestor.invocation_id
                    ));
                }
            }
            if let Some(dispatch_limit) = limits.dispatches_runs {
                let observed = ancestor.effects_observed.dispatches_runs.unwrap_or(0);
                if observed.saturating_add(dispatch_cost) > dispatch_limit {
                    return Err(format!(
                        "error.policy(code=budget_exhausted): ancestor {} allows dispatches_runs={dispatch_limit}, observed={observed}, requested={dispatch_cost}",
                        ancestor.invocation_id
                    ));
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
        Ok((dispatch_cost, effective_limits))
    }

    pub(super) fn atom_dispatch_cost_for_manifest(
        &self,
        manifest: &orchestration::atoms::types::AtomManifest,
    ) -> Result<u64, String> {
        match &manifest.implementation {
            orchestration::atoms::types::AtomImplementation::Profile { .. } => Ok(1),
            orchestration::atoms::types::AtomImplementation::Workflow { workflow_ref } => {
                let (workflow_name, _workflow_version) =
                    orchestration::atoms::validate::parse_typed_ref(workflow_ref, "workflow:")
                        .map_err(|e| format!("invalid workflow_ref: {e}"))?;
                let workflow = self
                    .resolve_workflow_by_id(&workflow_name)
                    .or_else(|| self.resolve_workflow_by_id(workflow_ref))
                    .ok_or_else(|| {
                        format!(
                            "workflow '{}' not found for atom dispatch budget",
                            workflow_ref
                        )
                    })?;
                self.workflow_static_dispatch_upper_bound(&workflow, 0)
            }
            orchestration::atoms::types::AtomImplementation::Deterministic { .. }
            | orchestration::atoms::types::AtomImplementation::Adapter { .. } => Ok(0),
        }
    }

    fn workflow_static_dispatch_upper_bound(
        &self,
        workflow: &workflow::Workflow,
        depth: u32,
    ) -> Result<u64, String> {
        if depth > workflow::engine::MAX_COMPOSITION_DEPTH {
            return Err(format!(
                "workflow '{}' exceeds static dispatch budget depth guard",
                workflow.name
            ));
        }
        let mut cost = 1u64;
        for (node_id, node) in &workflow.nodes {
            if !node.actor.is_empty() {
                cost = cost.saturating_add(1);
            }
            if let Some(sub) = &node.subworkflow {
                cost =
                    cost.saturating_add(self.workflow_static_dispatch_upper_bound(sub, depth + 1)?);
            }
            if let Some(sub_ref) = &node.subworkflow_ref {
                let sub = self.resolve_workflow_by_id(sub_ref).ok_or_else(|| {
                    format!(
                        "workflow '{}' node '{node_id}' references unknown subworkflow '{sub_ref}' while computing atom dispatch budget",
                        workflow.name
                    )
                })?;
                cost = cost
                    .saturating_add(self.workflow_static_dispatch_upper_bound(&sub, depth + 1)?);
            }
            if let Some(foreach) = &node.foreach {
                let Some(items) = foreach.items.as_array() else {
                    return Err(format!(
                        "workflow '{}' node '{node_id}' uses dynamic foreach items; atom dispatch budget must be statically bounded in v1",
                        workflow.name
                    ));
                };
                let child = self.fanout_child_dispatch_upper_bound(
                    &workflow.name,
                    node_id,
                    foreach.subworkflow.as_deref(),
                    foreach.subworkflow_ref.as_deref(),
                    depth + 1,
                )?;
                cost = cost.saturating_add(child.saturating_mul(items.len() as u64));
            }
            if let Some(matrix) = &node.matrix {
                let mut product = 1u64;
                for axis in &matrix.axes {
                    let Some(values) = axis.values.as_array() else {
                        return Err(format!(
                            "workflow '{}' node '{node_id}' uses dynamic matrix axis '{}'; atom dispatch budget must be statically bounded in v1",
                            workflow.name, axis.name
                        ));
                    };
                    product = product.saturating_mul(values.len() as u64);
                }
                let child = self.fanout_child_dispatch_upper_bound(
                    &workflow.name,
                    node_id,
                    matrix.subworkflow.as_deref(),
                    matrix.subworkflow_ref.as_deref(),
                    depth + 1,
                )?;
                cost = cost.saturating_add(child.saturating_mul(product));
            }
        }
        Ok(cost)
    }

    fn fanout_child_dispatch_upper_bound(
        &self,
        workflow_name: &str,
        node_id: &str,
        inline: Option<&workflow::Workflow>,
        referenced: Option<&str>,
        depth: u32,
    ) -> Result<u64, String> {
        if let Some(inline) = inline {
            return self.workflow_static_dispatch_upper_bound(inline, depth);
        }
        let Some(sub_ref) = referenced else {
            return Err(format!(
                "workflow '{workflow_name}' node '{node_id}' fanout has no child workflow"
            ));
        };
        let sub = self.resolve_workflow_by_id(sub_ref).ok_or_else(|| {
            format!(
                "workflow '{workflow_name}' node '{node_id}' references unknown fanout subworkflow '{sub_ref}' while computing atom dispatch budget"
            )
        })?;
        self.workflow_static_dispatch_upper_bound(&sub, depth)
    }

    pub(super) fn atom_invocation_ancestors(
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

    pub(super) fn record_child_invocation(
        &self,
        parent_id: Option<&str>,
        child_id: &str,
        dispatch_cost: u64,
    ) {
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
}

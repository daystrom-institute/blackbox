use std::sync::Arc;

use serde_json::Value;

use crate::server::SharedState;
use crate::tools::bro_helpers::resolve_actor_providers;
use crate::{orchestration, workflow};

/// Walk each ActorSpec.requires -> resolve actor brofiles/teams -> provider
/// capabilities. Empty `requires` is satisfied.
pub(crate) fn validate_workflow_capabilities(
    compiled: &workflow::CompiledWorkflow,
    state: &Arc<SharedState>,
) -> Result<(), String> {
    for (actor_name, actor) in &compiled.spec.actors {
        if actor.requires.is_empty() {
            continue;
        }
        let providers = resolve_actor_providers(actor, state)?;
        if providers.is_empty() {
            return Err(format!(
                "actor '{actor_name}' requires {:?} but resolves to no providers",
                actor.requires
            ));
        }
        if actor.runtime.is_some() {
            let satisfied = providers.iter().any(|provider| {
                let caps = provider.capabilities();
                actor.requires.iter().all(|r| caps.contains(r))
            });
            if !satisfied {
                return Err(format!(
                    "actor '{actor_name}' requires {:?} but no runtime candidate provider satisfies them",
                    actor.requires
                ));
            }
            continue;
        }
        for provider in &providers {
            let caps = provider.capabilities();
            let missing: Vec<_> = actor
                .requires
                .iter()
                .filter(|r| !caps.contains(r))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "actor '{actor_name}' requires {:?} but provider '{provider}' lacks {:?}",
                    actor.requires, missing
                ));
            }
        }
    }
    for (binding_name, binding) in &compiled.spec.atom_bindings {
        let manifest = resolve_atom_binding_manifest(binding, state)?;
        validate_atom_binding_limits(binding_name, binding, &manifest)?;
        if binding.requires.is_empty() {
            continue;
        }
        let providers = resolve_atom_binding_providers(binding, &manifest, state)?;
        if providers.is_empty() {
            return Err(format!(
                "atom binding '{binding_name}' requires {:?} but resolves to no providers",
                binding.requires
            ));
        }
        if manifest.runtime.is_some() {
            let satisfied = providers.iter().any(|provider| {
                let caps = provider.capabilities();
                binding.requires.iter().all(|r| caps.contains(r))
            });
            if !satisfied {
                return Err(format!(
                    "atom binding '{binding_name}' requires {:?} but no runtime candidate provider satisfies them",
                    binding.requires
                ));
            }
            continue;
        }
        for provider in &providers {
            let caps = provider.capabilities();
            let missing: Vec<_> = binding
                .requires
                .iter()
                .filter(|r| !caps.contains(r))
                .collect();
            if !missing.is_empty() {
                return Err(format!(
                    "atom binding '{binding_name}' requires {:?} but provider '{provider}' lacks {:?}",
                    binding.requires, missing
                ));
            }
        }
    }
    for (node_id, node) in &compiled.spec.nodes {
        if let Some(sub) = &node.subworkflow {
            let sub_compiled = workflow::compile((**sub).clone())
                .map_err(|e| format!("subworkflow on '{node_id}' compile: {e}"))?;
            validate_workflow_capabilities(&sub_compiled, state)
                .map_err(|e| format!("subworkflow on '{node_id}': {e}"))?;
        }
    }
    Ok(())
}

fn resolve_atom_binding_manifest(
    binding: &workflow::AtomBinding,
    state: &Arc<SharedState>,
) -> Result<orchestration::atoms::types::AtomManifest, String> {
    let catalog = state.artifacts.read();
    let reg = orchestration::atoms::registry::AtomRegistry::new(&catalog);
    let rec = reg
        .get(&binding.atom_ref)
        .map_err(|e| {
            format!(
                "atom binding '{}': registry lookup failed: {e}",
                binding.atom_ref
            )
        })?
        .ok_or_else(|| {
            format!(
                "atom binding references unknown atom '{}'",
                binding.atom_ref
            )
        })?;
    if !rec.active {
        return Err(format!(
            "atom binding references inactive atom '{}'",
            binding.atom_ref
        ));
    }
    rec.manifest.ok_or_else(|| {
        format!(
            "atom binding '{}' manifest parse failed: {}",
            binding.atom_ref,
            rec.manifest_parse_error.unwrap_or_default()
        )
    })
}

fn resolve_atom_binding_providers(
    binding: &workflow::AtomBinding,
    manifest: &orchestration::atoms::types::AtomManifest,
    state: &Arc<SharedState>,
) -> Result<Vec<orchestration::providers::Provider>, String> {
    if let Some(runtime) = &manifest.runtime {
        let config = orchestration::allocator::load_effective_config(&state.store_dir, None);
        return Ok(orchestration::allocator::provider_candidates_for_request(
            runtime, &config,
        ));
    }
    match &manifest.implementation {
        orchestration::atoms::types::AtomImplementation::Profile { brofile_ref } => {
            let (name, _version) =
                orchestration::atoms::validate::parse_typed_ref(brofile_ref, "brofile:")
                    .map_err(|e| e.to_string())?;
            let bf = orchestration::brofile::resolve_brofile(&name, &state.store_dir, None)
                .ok_or_else(|| {
                    format!(
                        "atom binding '{}' references missing brofile '{}'",
                        binding.atom_ref, name
                    )
                })?;
            Ok(vec![bf.provider])
        }
        orchestration::atoms::types::AtomImplementation::Workflow { .. } => Ok(Vec::new()),
        orchestration::atoms::types::AtomImplementation::Deterministic { .. } => Ok(Vec::new()),
        orchestration::atoms::types::AtomImplementation::Adapter { .. } => Ok(Vec::new()),
    }
}

fn validate_atom_binding_limits(
    binding_name: &str,
    binding: &workflow::AtomBinding,
    manifest: &orchestration::atoms::types::AtomManifest,
) -> Result<(), String> {
    let Some(limits) = &binding.limits else {
        return Ok(());
    };
    let Some(effects) = &manifest.effects else {
        return Ok(());
    };
    validate_binding_bool_limit(
        binding_name,
        "writes_files",
        effects.writes_files.as_ref(),
        limits.writes_files.as_ref(),
    )?;
    validate_binding_bool_limit(
        binding_name,
        "uses_network",
        effects.uses_network.as_ref(),
        limits.uses_network.as_ref(),
    )?;
    validate_binding_u64_limit(
        binding_name,
        "dispatches_runs",
        effects.dispatches_runs.as_ref(),
        limits.dispatches_runs.as_ref(),
    )?;
    validate_binding_u64_limit(
        binding_name,
        "max_depth",
        effects.max_depth.as_ref(),
        limits.max_depth.as_ref(),
    )
}

fn parse_effect_u64(value: Option<&Value>) -> Result<Option<u64>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) if s == "unbounded" => Ok(None),
        Some(Value::Number(n)) => n
            .as_u64()
            .map(Some)
            .ok_or_else(|| format!("invalid non-negative integer effect value: {n}")),
        Some(other) => Err(format!("invalid numeric effect value: {other}")),
    }
}

fn parse_effect_bool(value: Option<&Value>) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) if s == "unbounded" => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!("invalid boolean effect value: {other}")),
    }
}

fn validate_binding_u64_limit(
    binding_name: &str,
    field: &str,
    atom_value: Option<&Value>,
    binding_value: Option<&Value>,
) -> Result<(), String> {
    let atom_limit = parse_effect_u64(atom_value)?;
    let binding_limit = parse_effect_u64(binding_value)?;
    if let (Some(atom), Some(binding)) = (atom_limit, binding_limit)
        && binding > atom
    {
        return Err(format!(
            "atom binding '{binding_name}' limit {field}={binding} exceeds atom contract {field}={atom}"
        ));
    }
    if atom_limit.is_some() && binding_value.is_some() && binding_limit.is_none() {
        return Err(format!(
            "atom binding '{binding_name}' limit {field}=unbounded exceeds finite atom contract"
        ));
    }
    Ok(())
}

fn validate_binding_bool_limit(
    binding_name: &str,
    field: &str,
    atom_value: Option<&Value>,
    binding_value: Option<&Value>,
) -> Result<(), String> {
    let atom_limit = parse_effect_bool(atom_value)?;
    let binding_limit = parse_effect_bool(binding_value)?;
    if atom_limit == Some(false) && binding_limit == Some(true) {
        return Err(format!(
            "atom binding '{binding_name}' limit {field}=true exceeds atom contract {field}=false"
        ));
    }
    if atom_limit.is_some() && binding_value.is_some() && binding_limit.is_none() {
        return Err(format!(
            "atom binding '{binding_name}' limit {field}=unbounded exceeds finite atom contract"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn runtime_actor_workflow(
        providers: Vec<orchestration::providers::Provider>,
    ) -> workflow::CompiledWorkflow {
        workflow::compile(workflow::Workflow {
            name: "runtime-capabilities".into(),
            version: 1,
            actors: HashMap::from([(
                "worker".into(),
                workflow::ActorSpec {
                    kind: workflow::ActorKind::Executor,
                    brofile: None,
                    team: None,
                    durable: false,
                    compaction_anchor: false,
                    requires: vec![orchestration::providers::Capability::StructuredOutput],
                    runtime: Some(orchestration::allocator::RuntimeRequest {
                        pool: Some(orchestration::allocator::PoolRef {
                            name: None,
                            providers,
                        }),
                        ..Default::default()
                    }),
                },
            )]),
            atom_bindings: HashMap::new(),
            nodes: HashMap::from([(
                "run".into(),
                workflow::NodeSpec {
                    actor: "worker".into(),
                    prompt: Some("work".into()),
                    next: workflow::NodeTransition::Terminal,
                    ..Default::default()
                },
            )]),
            start: "run".into(),
            policy_packet: None,
            vars_schema: None,
            on_arc_exit: Vec::new(),
            on_arc_cancel: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn runtime_actor_capability_validation_accepts_mixed_pool_with_match() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::server::state::SharedState::for_test(tmp.path()));
        let compiled = runtime_actor_workflow(vec![
            orchestration::providers::Provider::Deepseek,
            orchestration::providers::Provider::Brodex,
        ]);

        validate_workflow_capabilities(&compiled, &state).unwrap();
    }

    #[test]
    fn runtime_actor_capability_validation_fails_when_no_candidate_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let state = Arc::new(crate::server::state::SharedState::for_test(tmp.path()));
        let compiled = runtime_actor_workflow(vec![orchestration::providers::Provider::Deepseek]);

        let err = validate_workflow_capabilities(&compiled, &state).unwrap_err();
        assert!(
            err.contains("no runtime candidate provider satisfies"),
            "{err}"
        );
    }
}

use crate::{orchestration, workflow};

pub(super) fn default_atom_owner() -> String {
    "operator:local".to_string()
}

pub(super) fn bounded_effect_u64(value: Option<&serde_json::Value>) -> Result<Option<u64>, String> {
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

pub(super) fn bounded_effect_bool(
    value: Option<&serde_json::Value>,
) -> Result<Option<bool>, String> {
    match value {
        None => Ok(None),
        Some(serde_json::Value::String(s)) if s == "unbounded" => Ok(None),
        Some(serde_json::Value::Bool(b)) => Ok(Some(*b)),
        Some(other) => Err(format!(
            "invalid boolean effect value (expected boolean or \"unbounded\"): {other}"
        )),
    }
}

pub(super) fn min_optional_u64(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(l), Some(r)) => Some(l.min(r)),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

pub(super) fn tighten_optional_bool(left: Option<bool>, right: Option<bool>) -> Option<bool> {
    match (left, right) {
        (Some(l), Some(r)) => Some(l && r),
        (Some(v), None) | (None, Some(v)) => Some(v),
        (None, None) => None,
    }
}

pub(super) fn effective_invocation_limits(
    effects: Option<&orchestration::atoms::types::AtomEffects>,
    binding_limits: Option<&workflow::AtomBindingLimits>,
) -> Result<orchestration::atoms::invocation::InvocationLimits, String> {
    let effect_writes = effects
        .map(|e| bounded_effect_bool(e.writes_files.as_ref()))
        .transpose()?
        .flatten();
    let effect_dispatches = effects
        .map(|e| bounded_effect_u64(e.dispatches_runs.as_ref()))
        .transpose()?
        .flatten();
    let effect_depth = effects
        .map(|e| bounded_effect_u64(e.max_depth.as_ref()))
        .transpose()?
        .flatten();
    let effect_network = effects
        .map(|e| bounded_effect_bool(e.uses_network.as_ref()))
        .transpose()?
        .flatten();

    let binding_writes = binding_limits
        .map(|l| bounded_effect_bool(l.writes_files.as_ref()))
        .transpose()?
        .flatten();
    let binding_dispatches = binding_limits
        .map(|l| bounded_effect_u64(l.dispatches_runs.as_ref()))
        .transpose()?
        .flatten();
    let binding_depth = binding_limits
        .map(|l| bounded_effect_u64(l.max_depth.as_ref()))
        .transpose()?
        .flatten();
    let binding_network = binding_limits
        .map(|l| bounded_effect_bool(l.uses_network.as_ref()))
        .transpose()?
        .flatten();

    Ok(orchestration::atoms::invocation::InvocationLimits {
        writes_files: tighten_optional_bool(effect_writes, binding_writes),
        dispatches_runs: min_optional_u64(effect_dispatches, binding_dispatches),
        max_depth: min_optional_u64(effect_depth, binding_depth),
        uses_network: tighten_optional_bool(effect_network, binding_network),
    })
}

pub(super) fn validate_atom_output(
    manifest: &orchestration::atoms::types::AtomManifest,
    data: &serde_json::Value,
) -> orchestration::atoms::invocation::OutputShapeStatus {
    let Some(outputs) = &manifest.outputs else {
        return orchestration::atoms::invocation::OutputShapeStatus::default();
    };
    let Some(schema) = &outputs.schema else {
        return orchestration::atoms::invocation::OutputShapeStatus::default();
    };
    let compiled = match jsonschema::JSONSchema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .compile(schema)
    {
        Ok(compiled) => compiled,
        Err(e) => {
            return orchestration::atoms::invocation::OutputShapeStatus {
                valid: Some(false),
                schema_ref: "outputs.schema".to_string(),
                errors: vec![format!("output schema failed to compile: {e}")],
            };
        }
    };
    match compiled.validate(data) {
        Ok(()) => orchestration::atoms::invocation::OutputShapeStatus {
            valid: Some(true),
            schema_ref: "outputs.schema".to_string(),
            errors: Vec::new(),
        },
        Err(errors) => orchestration::atoms::invocation::OutputShapeStatus {
            valid: Some(false),
            schema_ref: "outputs.schema".to_string(),
            errors: errors.map(|e| e.to_string()).collect(),
        },
    }
}

pub(super) fn atom_ref_allowed(allowed: &[String], atom_ref: &str) -> bool {
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

pub(super) fn sha256_json_value(value: &serde_json::Value) -> String {
    use sha2::Digest as _;

    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = sha2::Sha256::digest(bytes);
    format!("sha256:{}", hex::encode(digest))
}

pub(super) fn sha256_text(value: &str) -> String {
    use sha2::Digest as _;

    let digest = sha2::Sha256::digest(value.as_bytes());
    format!("sha256:{}", hex::encode(digest))
}

pub(super) fn iso_from_millis(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let nanos = ((ms % 1000) * 1_000_000) as u32;
    chrono::DateTime::from_timestamp(secs, nanos)
        .unwrap_or_else(chrono::Utc::now)
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}
